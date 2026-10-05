//! `ri install`, `remove` (`uninstall`), `update` and `list`: pi's package
//! commands (`package-manager-cli.ts` in pi `v1.0.0`).

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use ri_core::config::agent_dir;
use ri_core::extensions::discovery::is_native;
use ri_core::packages::PackageManager;
use ri_core::settings::{Scope, SettingsManager};
use ri_core::trust::{TrustStore, needs_prompt, resolve_trusted};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Install,
    Remove,
    Update,
    List,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Command::Install => "install",
            Command::Remove => "remove",
            Command::Update => "update",
            Command::List => "list",
        }
    }

    fn usage(self) -> &'static str {
        match self {
            Command::Install => "ri install <source> [-l] [--approve|--no-approve]",
            Command::Remove => "ri remove <source> [-l] [--approve|--no-approve]",
            Command::Update => {
                "ri update [source|self|ri] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve]"
            }
            Command::List => "ri list [--approve|--no-approve]",
        }
    }

    fn help(self) -> String {
        let usage = self.usage();
        match self {
            Command::Install => format!(
                "Usage:\n  {usage}\n\nInstall a package and add it to settings.\n\nOptions:\n  -l, --local       Install project-locally (.ri/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  ri install npm:@foo/bar\n  ri install git:github.com/user/repo\n  ri install git:git@github.com:user/repo\n  ri install https://github.com/user/repo\n  ri install ssh://git@github.com/user/repo\n  ri install ./local/path\n"
            ),
            Command::Remove => format!(
                "Usage:\n  {usage}\n\nRemove a package and its source from settings.\nAlias: ri uninstall <source> [-l]\n\nOptions:\n  -l, --local       Remove from project settings (.ri/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  ri remove npm:@foo/bar\n  ri uninstall npm:@foo/bar\n"
            ),
            Command::Update => format!(
                "Usage:\n  {usage}\n\nUpdate installed packages or model catalogs.\n\nOptions:\n  --self                  Update ri only (default when no target is given)\n  --extensions            Update installed packages only\n  --models                Refresh model catalogs only\n  --all                   Update ri and installed packages\n  --extension <source>    Update one package only\n  -a, --approve           Trust project-local files for this command\n  -na, --no-approve       Ignore project-local files for this command\n"
            ),
            Command::List => format!(
                "Usage:\n  {usage}\n\nList installed packages from user and project settings.\n\nOptions:\n  -a, --approve      Trust project-local files for this command\n  -na, --no-approve  Ignore project-local files for this command\n"
            ),
        }
    }
}

/// What to update.
#[derive(Debug, PartialEq, Eq)]
enum Update {
    /// ri itself.
    Myself,
    /// Packages: all, or the one named.
    Packages(Option<String>),
    /// Both.
    All,
    /// Model catalogs.
    Models,
}

#[derive(Debug, Default)]
struct Options {
    source: Option<String>,
    local: bool,
    trust: Option<bool>,
    help: bool,
    invalid_option: Option<String>,
    invalid_argument: Option<String>,
    missing_value: Option<String>,
    self_flag: bool,
    extensions_flag: bool,
    all_flag: bool,
    models_flag: bool,
    extension: Option<String>,
    /// pi's `conflictingOptions`: the first conflict between flags.
    conflict: Option<String>,
}

fn parse(command: Command, args: &[String]) -> Options {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        let local_command = matches!(command, Command::Install | Command::Remove);
        match arg {
            "-h" | "--help" => options.help = true,
            "-l" | "--local" if local_command => options.local = true,
            "--self" if command == Command::Update => options.self_flag = true,
            "--extensions" if command == Command::Update => options.extensions_flag = true,
            "--all" if command == Command::Update => options.all_flag = true,
            "--models" if command == Command::Update => options.models_flag = true,
            "-a" | "--approve" => options.trust = Some(true),
            "-na" | "--no-approve" => options.trust = Some(false),
            "--extension" if command == Command::Update => match args.get(index) {
                Some(_) if options.extension.is_some() => {
                    options
                        .conflict
                        .get_or_insert_with(|| "--extension can only be provided once".into());
                    index += 1;
                }
                Some(value) if !value.starts_with('-') => {
                    options.extension = Some(value.clone());
                    index += 1;
                }
                _ => {
                    options.missing_value.get_or_insert_with(|| arg.to_owned());
                }
            },
            _ if arg.starts_with('-') => {
                options.invalid_option.get_or_insert_with(|| arg.to_owned());
            }
            _ if options.source.is_none() => options.source = Some(arg.to_owned()),
            _ => {
                options
                    .invalid_argument
                    .get_or_insert_with(|| arg.to_owned());
            }
        }
    }
    if command == Command::Update {
        let conflict = update_conflict(&options);
        if options.conflict.is_none() {
            options.conflict = conflict;
        }
    }
    options
}

/// pi's checks for update flags that cannot be combined.
fn update_conflict(options: &Options) -> Option<String> {
    let source = options.source.is_some();
    let extension = options.extension.is_some();
    let (myself, extensions, all, models) = (
        options.self_flag,
        options.extensions_flag,
        options.all_flag,
        options.models_flag,
    );
    let conflict = if all && (myself || extensions || models || extension) {
        "--all cannot be combined with --self, --extensions, --models, or --extension"
    } else if all && source {
        "--all cannot be combined with a positional source"
    } else if models && (myself || extensions || all || extension) {
        "--models cannot be combined with --self, --extensions, --all, or --extension"
    } else if models && source {
        "--models cannot be combined with a positional source"
    } else if models {
        return None;
    } else if extension && (myself || extensions || all) {
        "--extension cannot be combined with --self, --extensions, or --all"
    } else if extension && source {
        "--extension cannot be combined with a positional source"
    } else if !extension
        && source
        && !matches!(options.source.as_deref(), Some("self" | "ri" | "pi"))
        && (extensions || myself || all)
    {
        "positional update targets cannot be combined with --self, --extensions, or --all"
    } else {
        return None;
    };
    Some(conflict.to_owned())
}

fn update_target(options: &Options) -> Update {
    if options.models_flag {
        return Update::Models;
    }
    if let Some(source) = &options.extension {
        return Update::Packages(Some(source.clone()));
    }
    match options.source.as_deref() {
        Some("self" | "ri" | "pi") if options.extensions_flag => Update::All,
        Some("self" | "ri" | "pi") => Update::Myself,
        Some(source) => Update::Packages(Some(source.to_owned())),
        None if options.all_flag || (options.self_flag && options.extensions_flag) => Update::All,
        None if options.extensions_flag => Update::Packages(None),
        None => Update::Myself,
    }
}

fn out(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn err(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// pi's `createCommandSettingsManager`: settings whose project scope loads
/// when `override_`, a stored decision or `defaultProjectTrust` trusts the
/// project, or when the user trusts it at pi's prompt in a terminal.
pub fn command_settings(
    cwd: &Path,
    agent_dir: &Path,
    override_: Option<bool>,
) -> anyhow::Result<SettingsManager> {
    let global = SettingsManager::load(agent_dir, cwd, false)?;
    let view = global.settings();
    let store = TrustStore::new(agent_dir);
    let default = view.default_project_trust;
    let trusted = if std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && needs_prompt(cwd, &store, override_, default)
    {
        crate::interactive::picker::ask_project_trust(agent_dir, cwd, view.theme.as_deref())?
            .unwrap_or(false)
    } else {
        resolve_trusted(cwd, &store, override_, default)
    };
    Ok(SettingsManager::load(agent_dir, cwd, trusted)?)
}

/// Runs a package command when `args` starts with one; its exit code.
pub async fn run(args: &[String]) -> Option<u8> {
    let command = match args.first()?.as_str() {
        "install" => Command::Install,
        "remove" | "uninstall" => Command::Remove,
        "update" => Command::Update,
        "list" => Command::List,
        _ => return None,
    };
    let options = parse(command, &args[1..]);
    if options.help {
        // pi's console.log adds a newline after the text's own.
        let _ = writeln!(std::io::stdout(), "{}", command.help());
        return Some(0);
    }
    let usage = command.usage();
    if let Some(option) = &options.invalid_option {
        err(&format!(
            "Unknown option {option} for \"{}\".",
            command.name()
        ));
        err(&format!("Use \"ri --help\" or \"{usage}\"."));
        return Some(1);
    }
    if let Some(option) = &options.missing_value {
        err(&format!("Missing value for {option}."));
        err(&format!("Usage: {usage}"));
        return Some(1);
    }
    if let Some(argument) = &options.invalid_argument {
        err(&format!("Unexpected argument {argument}."));
        err(&format!("Usage: {usage}"));
        return Some(1);
    }
    if let Some(conflict) = &options.conflict {
        err(conflict);
        err(&format!("Usage: {usage}"));
        return Some(1);
    }
    if matches!(command, Command::Install | Command::Remove)
        && options.source.as_deref().is_none_or(str::is_empty)
    {
        err(&format!("Missing {} source.", command.name()));
        err(&format!("Usage: {usage}"));
        return Some(1);
    }
    if command == Command::Update && update_target(&options) == Update::Models {
        return Some(refresh_models().await);
    }
    Some(execute(command, options).await)
}

/// `ri update --models`: pi's `refreshModelCatalogs`, which fetches every
/// configured catalog, offline setting or not, within 15 seconds.
async fn refresh_models() -> u8 {
    let registry = ri_ai::registry::ModelRegistry::load(&agent_dir());
    let Some(store) = registry.models_store().cloned() else {
        return 0;
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let options = ri_ai::model_catalog::RefreshOptions {
        force: true,
        cancel: cancel.clone(),
        ..Default::default()
    };
    let timer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        cancel.cancel();
    });
    let targets = registry.catalog_targets().await;
    let refreshed = ri_ai::model_catalog::refresh(&targets, &store, &options).await;
    timer.abort();
    let failure = if refreshed.aborted {
        Some("Model catalog refresh timed out.".to_owned())
    } else if !refreshed.errors.is_empty() {
        let details: Vec<String> = refreshed
            .errors
            .iter()
            .map(|(provider, error)| format!("{provider}: {error}"))
            .collect();
        Some(format!(
            "Could not refresh model catalogs: {}",
            details.join("; ")
        ))
    } else {
        None
    };
    match failure {
        Some(message) => {
            err(&format!("Error: {message}"));
            1
        }
        None => {
            out("Model catalogs refreshed");
            0
        }
    }
}

async fn execute(command: Command, options: Options) -> u8 {
    let Ok(cwd) = std::env::current_dir() else {
        err("Error: cannot read the working directory");
        return 1;
    };
    let agent_dir = agent_dir();
    let settings = match command_settings(&cwd, &agent_dir, options.trust) {
        Ok(settings) => settings,
        Err(error) => {
            err(&format!("Error: {error}"));
            return 1;
        }
    };
    for error in settings.errors() {
        err(&format!("Warning: {error}"));
    }
    let mut packages = PackageManager::new(
        cwd,
        agent_dir,
        settings,
        ri_core::packages::npm::default_registry(),
    );
    packages.on_progress(out);
    let source = options.source.clone().unwrap_or_default();
    let result = match command {
        Command::Install => packages
            .install(&source, options.local)
            .await
            .map(|()| out(&format!("Installed {source}"))),
        Command::Remove => match packages.remove(&source, options.local).await {
            Ok(true) => {
                out(&format!("Removed {source}"));
                Ok(())
            }
            Ok(false) => {
                err(&format!("No matching package found for {source}"));
                return 1;
            }
            Err(error) => Err(error),
        },
        Command::List => {
            list(&packages);
            Ok(())
        }
        Command::Update => {
            let target = update_target(&options);
            if target == Update::Myself && options.source.is_none() && !options.self_flag {
                out("Extensions are skipped. Run ri update --extensions to update extensions.");
            }
            let packages_result = match &target {
                Update::Packages(source) => packages.update(source.as_deref()).await.map(|()| {
                    out(&source.as_ref().map_or_else(
                        || "Updated packages".to_owned(),
                        |source| format!("Updated {source}"),
                    ));
                }),
                Update::All => packages
                    .update(None)
                    .await
                    .map(|()| out("Updated packages")),
                Update::Myself | Update::Models => Ok(()),
            };
            if packages_result.is_ok() && matches!(target, Update::Myself | Update::All) {
                err("ri cannot update itself; install the latest release instead.");
                return 1;
            }
            packages_result
        }
    };
    match result {
        Ok(()) => 0,
        // pi prints this one without the prefix.
        Err(error @ ri_core::packages::PackageError::Untrusted) => {
            err(&error.to_string());
            1
        }
        Err(error) => {
            err(&format!("Error: {error}"));
            1
        }
    }
}

/// The kinds of a package's extensions, as `list` tags them: `[npm]` for
/// pi extensions, which run in ri-js, and `[wasm]` for native ones. Empty
/// for a package without extensions.
fn kinds(extensions: &[PathBuf]) -> &'static str {
    let native = extensions.iter().any(|path| is_native(path));
    let js = extensions.iter().any(|path| !is_native(path));
    match (js, native) {
        (true, true) => " [npm, wasm]",
        (true, false) => " [npm]",
        (false, true) => " [wasm]",
        (false, false) => "",
    }
}

fn list(packages: &PackageManager) {
    let configured = packages.list();
    if configured.is_empty() {
        out("No packages installed.");
        return;
    }
    let print = |scope: Scope| {
        for package in configured.iter().filter(|package| package.scope == scope) {
            let filtered = if package.filtered { " (filtered)" } else { "" };
            let kinds = kinds(&package.extensions);
            out(&format!("  {}{kinds}{filtered}", package.source));
            if let Some(path) = &package.installed_path {
                out(&format!("    {}", path.display()));
            }
        }
    };
    let user = configured
        .iter()
        .any(|package| package.scope == Scope::Global);
    let project = configured
        .iter()
        .any(|package| package.scope == Scope::Project);
    if user {
        out("User packages:");
        print(Scope::Global);
    }
    if project {
        if user {
            out("");
        }
        out("Project packages:");
        print(Scope::Project);
    }
}
