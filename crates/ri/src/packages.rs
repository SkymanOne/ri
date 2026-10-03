//! `ri install`, `remove` (`uninstall`), `update` and `list`: pi's package
//! commands (`package-manager-cli.ts` in pi `v1.0.0`).

use std::io::Write as _;

use ri_core::config::agent_dir;
use ri_core::packages::PackageManager;
use ri_core::settings::{Scope, SettingsManager};
use ri_core::trust::{TrustStore, resolve_trusted};

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
                "ri update [source|self|ri] [--self|--extensions|--all] [--extension <source>] [--approve|--no-approve]"
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
                "Usage:\n  {usage}\n\nUpdate installed packages.\n\nOptions:\n  --self                  Update ri only (default when no target is given)\n  --extensions            Update installed packages only\n  --all                   Update ri and installed packages\n  --extension <source>    Update one package only\n  -a, --approve           Trust project-local files for this command\n  -na, --no-approve       Ignore project-local files for this command\n"
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
    extension: Option<String>,
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
            "-a" | "--approve" => options.trust = Some(true),
            "-na" | "--no-approve" => options.trust = Some(false),
            "--extension" if command == Command::Update => match args.get(index) {
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
    options
}

fn update_target(options: &Options) -> Update {
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
        let _ = write!(std::io::stdout(), "{}", command.help());
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
    if matches!(command, Command::Install | Command::Remove) && options.source.is_none() {
        err(&format!("Missing {} source.", command.name()));
        err(&format!("Usage: {usage}"));
        return Some(1);
    }
    Some(execute(command, options).await)
}

async fn execute(command: Command, options: Options) -> u8 {
    let Ok(cwd) = std::env::current_dir() else {
        err("Error: cannot read the working directory");
        return 1;
    };
    let agent_dir = agent_dir();
    let trust = TrustStore::new(&agent_dir);
    let settings = match SettingsManager::load(&agent_dir, &cwd, false).and_then(|global| {
        let trusted = resolve_trusted(
            &cwd,
            &trust,
            options.trust,
            global.settings().default_project_trust,
        );
        SettingsManager::load(&agent_dir, &cwd, trusted)
    }) {
        Ok(settings) => settings,
        Err(error) => {
            err(&format!("Error: {error}"));
            return 1;
        }
    };
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
                Update::Myself => Ok(()),
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
        Err(error) => {
            err(&format!("Error: {error}"));
            1
        }
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
            out(&format!("  {}{filtered}", package.source));
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
