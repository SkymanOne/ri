//! `yapi install`, `remove` (`uninstall`), `update` and `list`: pi's package
//! commands (`package-manager-cli.ts` in pi `v1.0.0`).

use std::io::Write as _;
use std::path::PathBuf;

use yapi_core::config::agent_dir;
use yapi_core::extensions::discovery::is_native;
use yapi_core::packages::PackageManager;
use yapi_core::settings::Scope;

use crate::{err, out};

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
            Command::Install => "yapi install <source> [-l] [--approve|--no-approve]",
            Command::Remove => "yapi remove <source> [-l] [--approve|--no-approve]",
            Command::Update => {
                "yapi update [source|self|yapi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
            }
            Command::List => "yapi list [--approve|--no-approve]",
        }
    }

    fn help(self) -> String {
        let usage = self.usage();
        match self {
            Command::Install => format!(
                "Usage:\n  {usage}\n\nInstall a package and add it to settings.\n\nOptions:\n  -l, --local       Install project-locally (.yapi/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  yapi install npm:@foo/bar\n  yapi install git:github.com/user/repo\n  yapi install git:git@github.com:user/repo\n  yapi install https://github.com/user/repo\n  yapi install ssh://git@github.com/user/repo\n  yapi install ./local/path\n"
            ),
            Command::Remove => format!(
                "Usage:\n  {usage}\n\nRemove a package and its source from settings.\nAlias: yapi uninstall <source> [-l]\n\nOptions:\n  -l, --local       Remove from project settings (.yapi/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  yapi remove npm:@foo/bar\n  yapi uninstall npm:@foo/bar\n"
            ),
            Command::Update => format!(
                "Usage:\n  {usage}\n\nUpdate installed packages or model catalogs.\n\nOptions:\n  --self                  Update yapi only (default when no target is given)\n  --extensions            Update installed packages only\n  --models                Refresh model catalogs only\n  --all                   Update yapi and installed packages\n  --extension <source>    Update one package only\n  -a, --approve           Trust project-local files for this command\n  -na, --no-approve       Ignore project-local files for this command\n  --force                 No effect: yapi updates only through its installer\n"
            ),
            Command::List => format!(
                "Usage:\n  {usage}\n\nList installed packages from user and project settings.\n\nOptions:\n  -a, --approve      Trust project-local files for this command\n  -na, --no-approve  Ignore project-local files for this command\n"
            ),
        }
    }
}

/// What to update.
#[derive(Debug, Default, PartialEq, Eq)]
enum Update {
    /// yapi itself.
    #[default]
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
    /// What `update` updates.
    target: Update,
}

/// What `yapi update` says instead of updating yapi.
const SELF_UPDATE: &str = "yapi cannot update itself. Install the latest release the way you installed yapi, such as:\n  curl -fsSL https://raw.githubusercontent.com/SkymanOne/yapi/main/install.sh | sh";

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
            // pi's `--force` reinstalls pi itself, which yapi leaves to its
            // installer.
            "--force" if command == Command::Update => {}
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
        let (target, conflict) = update_target(&options);
        options.target = target;
        if options.conflict.is_none() {
            options.conflict = conflict.map(str::to_owned);
        }
    }
    options
}

/// pi's update target and first conflict between update flags, worked out
/// in one pass as pi's `parsePackageCommand` does.
fn update_target(options: &Options) -> (Update, Option<&'static str>) {
    let (myself, extensions, all, models) = (
        options.self_flag,
        options.extensions_flag,
        options.all_flag,
        options.models_flag,
    );
    let (source, extension) = (options.source.as_deref(), options.extension.as_deref());
    let mut conflict = None;
    let mut note = |text: &'static str| {
        conflict.get_or_insert(text);
    };
    if all && (myself || extensions || models || extension.is_some()) {
        note("--all cannot be combined with --self, --extensions, --models, or --extension");
    }
    if all && source.is_some() {
        note("--all cannot be combined with a positional source");
    }
    let target = if models {
        if myself || extensions || all || extension.is_some() {
            note("--models cannot be combined with --self, --extensions, --all, or --extension");
        }
        if source.is_some() {
            note("--models cannot be combined with a positional source");
        }
        Update::Models
    } else if let Some(extension) = extension {
        if myself || extensions || all {
            note("--extension cannot be combined with --self, --extensions, or --all");
        }
        if source.is_some() {
            note("--extension cannot be combined with a positional source");
        }
        Update::Packages(Some(extension.to_owned()))
    } else if let Some(source) = source {
        if matches!(source, "self" | "yapi" | "pi") {
            if extensions {
                Update::All
            } else {
                Update::Myself
            }
        } else {
            if extensions || myself || all {
                note(
                    "positional update targets cannot be combined with --self, --extensions, or --all",
                );
            }
            Update::Packages(Some(source.to_owned()))
        }
    } else if all || (myself && extensions) {
        Update::All
    } else if extensions {
        Update::Packages(None)
    } else {
        Update::Myself
    };
    (target, conflict)
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
        err(&format!("Use \"yapi --help\" or \"{usage}\"."));
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
    if command == Command::Update && options.target == Update::Models {
        return Some(refresh_models().await);
    }
    Some(execute(command, options).await)
}

/// `yapi update --models`: pi's `refreshModelCatalogs`, which fetches every
/// configured catalog, offline setting or not, within 15 seconds.
async fn refresh_models() -> u8 {
    let registry = yapi_ai::registry::ModelRegistry::load(&agent_dir());
    let Some(store) = registry.models_store().cloned() else {
        return 0;
    };
    use yapi_ai::model_catalog::{REFRESH_TIMEOUT, RefreshOptions, cancel_after, refresh};
    let options = RefreshOptions {
        force: true,
        cancel: cancel_after(REFRESH_TIMEOUT),
        ..Default::default()
    };
    let targets = registry.catalog_targets().await;
    let refreshed = refresh(&targets, &store, &options).await;
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
    let settings = match crate::startup::load_settings(&cwd, &agent_dir, options.trust, true) {
        Ok((settings, _)) => settings,
        Err(error) => {
            err(&format!("Error: {error}"));
            return 1;
        }
    };
    for error in settings.errors() {
        err(&format!("Warning: {error}"));
    }
    let mut packages =
        PackageManager::new(cwd, agent_dir, settings, yapi_core::packages::npm::config());
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
            let target = &options.target;
            if *target == Update::Myself && options.source.is_none() && !options.self_flag {
                out("Extensions are skipped. Run yapi update --extensions to update extensions.");
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
                err(SELF_UPDATE);
                return 1;
            }
            packages_result
        }
    };
    match result {
        Ok(()) => 0,
        // pi prints this one without the prefix.
        Err(error @ yapi_core::packages::PackageError::Untrusted) => {
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
/// pi extensions, which run in yapi-js, and `[wasm]` for native ones. Empty
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
