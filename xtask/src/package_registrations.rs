//! `cargo xtask package-registrations`: installs the most-downloaded npm pi
//! packages (`tests/fixtures/pi/packages/ranked.json`) with yapi's npm client,
//! loads their extensions, and compares what they register with pi's
//! (`registrations.json`, from `packages.mjs` in the fixture generator).
//!
//! The comparison counts the first [`COUNTED`] packages in the list's order
//! that pi itself installs and loads in its sandbox, so every counted package
//! has pi's result to compare with. A package that fails in yapi counts as a
//! difference. A package works without errors when it matches and neither
//! side reports a load error.
//!
//! Needs network access to the npm registry. Extensions run without network
//! or process access, see only their scratch directory, and get the same five
//! environment variables as pi's side.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};
use yapi_core::packages::{PackageManager, npm::default_registry};
use yapi_core::settings::SettingsManager;
use yapi_ext::{Engine, ExtensionHost, Grants, Options};
use yapi_types::rpc::SourceInfo;

const FIXTURES: &str = "tests/fixtures/pi/packages";
/// Packages the comparison counts.
const COUNTED: usize = 500;

/// Compare yapi's registrations of the top npm pi packages with pi's.
#[derive(clap::Args)]
pub struct Args {
    /// Only these packages, separated by commas. A new run's results replace
    /// theirs in the saved run.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Print load errors in full.
    #[arg(long)]
    verbose: bool,
    /// Packages installed and loaded at once.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Compare the registrations a previous run saved, without loading the
    /// packages again.
    #[arg(long)]
    compare_only: bool,
    /// Skip the packages the saved run already has.
    #[arg(long)]
    resume: bool,
    /// Measure the package at this index of the list and print the result.
    /// Each package runs in a child process of its own, as on pi's side, so
    /// one package's memory or crash stays with it.
    #[arg(long, hide = true)]
    measure: Option<usize>,
}

/// Precedes a child's result on its stdout, which extensions may write to.
const MARKER: &str = "@@registrations@@";

/// Installs and loads the package at `index`, at `version`, in a child
/// process of `exe`.
async fn measure(exe: &Path, index: usize, version: &str, verbose: bool) -> Value {
    let crash = |message: String| json!({"version": version, "crash": message});
    let mut command = tokio::process::Command::new(exe);
    command
        .args(["package-registrations", "--measure", &index.to_string()])
        .args(verbose.then_some("--verbose"))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // pi's side allows five minutes to install and two to load.
    let output = match tokio::time::timeout(Duration::from_secs(420), command.output()).await {
        Err(_) => return crash("timed out".into()),
        Ok(Err(err)) => return crash(err.to_string()),
        Ok(Ok(output)) => output,
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    if verbose {
        eprint!("{stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .rfind(MARKER)
        .and_then(|at| serde_json::from_str(&stdout[at + MARKER.len()..]).ok())
        .unwrap_or_else(|| {
            let tail: Vec<&str> = stderr
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            crash(format!(
                "{}: {}",
                output.status,
                tail[tail.len().saturating_sub(3)..].join(" ")
            ))
        })
}

fn dump(loaded: &Value) -> Value {
    let list = |key: &str| loaded[key].as_array().cloned().unwrap_or_default();
    json!({
        "tools": list("tools").iter().map(|tool| json!({
            "name": tool["name"], "label": tool["label"],
            "description": tool["description"], "parameters": tool["parameters"],
        })).collect::<Vec<_>>(),
        "commands": list("commands").iter().map(|command| json!({
            "name": command["name"], "description": command["description"],
        })).collect::<Vec<_>>(),
        "flags": list("flags"),
        "shortcuts": list("shortcuts"),
        "events": list("events"),
    })
}

async fn load(
    engine: &Engine,
    dir: &Path,
    name: &str,
    version: &str,
    verbose: bool,
) -> anyhow::Result<Value> {
    let agent = dir.join("agent");
    let cwd = dir.join("project");
    std::fs::create_dir_all(&agent)?;
    std::fs::create_dir_all(&cwd)?;
    std::fs::write(
        agent.join("settings.json"),
        json!({"packages": [format!("npm:{name}@{version}")]}).to_string(),
    )?;
    let settings = SettingsManager::load(&agent, &cwd, false)?;
    let mut packages =
        PackageManager::new(cwd.clone(), agent.clone(), settings, default_registry());
    let mut install_errors = Vec::new();
    packages
        .install_missing(|error| install_errors.push(error))
        .await;
    if let Some(error) = install_errors.first() {
        return Ok(json!({"version": version, "install": error}));
    }
    let entries: Vec<PathBuf> = packages
        .list()
        .into_iter()
        .flat_map(|package| package.extensions)
        .collect();
    let sources: Vec<SourceInfo> = entries
        .iter()
        .map(|path| SourceInfo {
            path: path.to_string_lossy().into_owned(),
            source: format!("npm:{name}@{version}"),
            scope: "user".into(),
            origin: "package".into(),
            base_dir: None,
        })
        .collect();
    let mut options = Options::new(cwd.clone());
    options.agent_dir = agent.clone();
    // As pi's harness sets HOME and TMPDIR.
    options.temp_dir = dir.to_path_buf();
    options.home_dir = dir.join("home");
    std::fs::create_dir_all(&options.home_dir)?;
    options.cache_dir = Some(dir.join("cache"));
    options.grants = Grants {
        filesystem: true,
        process: false,
        network: false,
        environment: true,
    };
    // The variables pi's side gets, and nothing from this process.
    options.environment = Some(
        [
            ("PATH", std::env::var("PATH").unwrap_or_default()),
            ("HOME", options.home_dir.to_string_lossy().into_owned()),
            ("TMPDIR", dir.to_string_lossy().into_owned()),
            ("PI_OFFLINE", "1".to_owned()),
            ("PI_CODING_AGENT_DIR", agent.to_string_lossy().into_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect(),
    );
    options.filesystem_roots = vec![dir.to_path_buf()];
    let (extensions, errors) = if sources.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let host = ExtensionHost::load(engine, options, &sources).await?;
        if verbose {
            for error in host.errors() {
                eprintln!("{}: {}", error.path.display(), error.error);
            }
        }
        (
            host.registrations().iter().map(dump).collect::<Vec<_>>(),
            host.errors()
                .iter()
                .map(|error| error.error.lines().next().unwrap_or_default().to_owned())
                .collect::<Vec<_>>(),
        )
    };
    let text = json!({
        "version": version,
        "entries": entries,
        "extensions": extensions,
        "errors": errors,
    })
    .to_string()
    .replace(&*agent.to_string_lossy(), "<agent>")
    .replace(&*cwd.to_string_lossy(), "<cwd>")
    .replace(".yapi/", ".pi/");
    Ok(serde_json::from_str(&text)?)
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    tokio::runtime::Runtime::new()?.block_on(compare(args))
}

/// `value` without codemode's `models` line, which points into pi's install
/// for pi and into the agent directory for yapi (docs/compat.md).
fn without_models_line(value: Value) -> Value {
    static LINE: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(
            r"(?s)\n- `models`: classifiers and image generation\. Read .*? first\.",
        )
        .expect("the pattern is valid")
    });
    match value {
        Value::String(text) => Value::String(LINE.replace_all(&text, "").into_owned()),
        Value::Array(items) => Value::Array(items.into_iter().map(without_models_line).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, item)| (key, without_models_line(item)))
                .collect(),
        ),
        other => other,
    }
}

/// How one package compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    /// yapi registers what pi registers.
    Match,
    /// pi itself fails to install or load the package, or Node's permission
    /// model, which sandboxes pi's side, stops one of its extensions.
    PiFails,
    /// yapi's npm client fails where pi's install succeeds.
    RiInstall,
    /// The registrations differ, or yapi fails to load an installed package.
    Differs,
}

fn outcome(got: &Value, want: &Value) -> Outcome {
    let sandboxed = want["errors"].as_array().is_some_and(|errors| {
        errors.iter().any(|error| {
            error
                .as_str()
                .is_some_and(|error| error.contains("Access to this API has been restricted"))
        })
    });
    if want.is_null() || want.get("crash").is_some() || want.get("install").is_some() || sandboxed {
        return Outcome::PiFails;
    }
    if got.get("install").is_some() {
        return Outcome::RiInstall;
    }
    let same = ["extensions", "errors"]
        .iter()
        .all(|key| got.get(*key) == want.get(*key));
    if same {
        Outcome::Match
    } else {
        Outcome::Differs
    }
}

async fn compare(args: Args) -> anyhow::Result<ExitCode> {
    use futures_util::StreamExt as _;

    let fixtures = Path::new(FIXTURES);
    let top: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(fixtures.join("ranked.json"))?)?;
    std::fs::create_dir_all("target/package-registrations")?;
    let scratch = Path::new("target/package-registrations").canonicalize()?;
    let selected: Vec<(usize, String, String)> = top
        .iter()
        .enumerate()
        .map(|(index, package)| {
            let text = |key: &str| package[key].as_str().unwrap_or_default().to_owned();
            (index, text("name"), text("version"))
        })
        .filter(|(_, name, _)| args.only.is_empty() || args.only.contains(name))
        .collect();
    let saved = scratch.join("registrations-yapi.json");
    if let Some(index) = args.measure {
        let (_, name, version) = selected
            .iter()
            .find(|(at, _, _)| *at == index)
            .context("no package at that index")?;
        let engine = Engine::new(Some(&scratch.join("wasm-cache")))?;
        let dir = scratch.join(index.to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let got = load(&engine, &dir, name, version, args.verbose)
            .await
            .unwrap_or_else(|err| json!({"version": version, "crash": err.to_string()}));
        // The installs are large; only the results are kept.
        let _ = std::fs::remove_dir_all(&dir);
        println!("\n{MARKER}{}", yapi_types::json::to_string(&got)?);
        return Ok(ExitCode::SUCCESS);
    }
    let previous: BTreeMap<String, Value> = std::fs::read_to_string(&saved)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let results: Vec<(String, Value)> = if args.compare_only {
        anyhow::ensure!(!previous.is_empty(), "no saved run");
        previous
            .into_iter()
            .filter(|(name, _)| selected.iter().any(|(_, selected, _)| selected == name))
            .collect()
    } else {
        // Saved as each package finishes, so an interrupted run keeps what it
        // measured, and `--only` runs replace their packages in the last run.
        let keep = !args.only.is_empty() || args.resume;
        let selected: Vec<_> = selected
            .into_iter()
            .filter(|(_, name, _)| !args.resume || !previous.contains_key(name))
            .collect();
        let all = std::sync::Mutex::new(if keep { previous } else { BTreeMap::new() });
        // Found once: a rebuild during the run replaces the file, after which
        // the running executable's own path reads as deleted.
        let exe = std::env::current_exe()?;
        let total = selected.len();
        let done = std::sync::atomic::AtomicUsize::new(0);
        futures_util::stream::iter(selected)
            .map(|(index, name, version)| {
                let (all, done, saved, verbose, exe) = (&all, &done, &saved, args.verbose, &exe);
                async move {
                    let got = measure(exe, index, &version, verbose).await;
                    let mut all = all
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    all.insert(name.clone(), got.clone());
                    // Replaced whole, so a reader never sees half a file.
                    if let Ok(text) = yapi_types::json::to_string_pretty(&*all, "\t") {
                        let partial = saved.with_extension("json.partial");
                        if std::fs::write(&partial, text).is_ok() {
                            let _ = std::fs::rename(&partial, saved);
                        }
                    }
                    let done = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    eprintln!("[{done}/{total}] {name}");
                    (name, got)
                }
            })
            .buffer_unordered(args.jobs.max(1))
            .collect()
            .await
    };
    // Read last, so pi's side can be regenerated while yapi's runs.
    let expected: BTreeMap<String, Value> = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join("registrations.json")).context(
            "run `node packages.mjs > ../packages/registrations.json` in the fixture generator",
        )?,
    )?;
    let mut outcomes: BTreeMap<Outcome, Vec<String>> = BTreeMap::new();
    let mut results: BTreeMap<String, Value> = results.into_iter().collect();
    let mut counted = 0;
    // Counted packages that work without errors.
    let mut working = 0;
    // In the list's order: the first COUNTED packages pi runs count, the
    // packages pi cannot run are skipped, and the rest are not counted.
    for package in &top {
        let name = package["name"].as_str().unwrap_or_default();
        let Some(got) = results.remove(name) else {
            continue;
        };
        if counted == COUNTED {
            break;
        }
        let want = without_models_line(expected.get(name).cloned().unwrap_or(Value::Null));
        let result = outcome(&got, &want);
        let version = got["version"].as_str().unwrap_or_default();
        eprintln!(
            "{} {name}@{version}",
            match result {
                Outcome::Match => "ok      ",
                Outcome::PiFails => "pi-fails",
                Outcome::RiInstall => "install ",
                Outcome::Differs => "DIFFER  ",
            }
        );
        if result != Outcome::PiFails {
            counted += 1;
            // A match has pi's errors, so neither reports any.
            working += usize::from(
                result == Outcome::Match && got["errors"].as_array().is_none_or(Vec::is_empty),
            );
        }
        outcomes.entry(result).or_default().push(name.to_owned());
    }
    let count = |outcome| outcomes.get(&outcome).map_or(0, Vec::len);
    eprintln!(
        "{counted} packages: {} match pi, {} differ, {} fail to install in yapi. Skipped {} that fail in pi itself: {}",
        count(Outcome::Match),
        count(Outcome::Differs),
        count(Outcome::RiInstall),
        count(Outcome::PiFails),
        outcomes
            .get(&Outcome::PiFails)
            .map(|names| names.join(", "))
            .unwrap_or_default(),
    );
    if counted > 0 {
        eprintln!(
            "{} of {counted} packages match ({:.1}%); yapi's registrations are in {}",
            count(Outcome::Match),
            100.0 * count(Outcome::Match) as f64 / counted as f64,
            saved.display()
        );
        eprintln!(
            "{working} of {counted} packages work without errors: they load in yapi without errors and register what pi registers ({:.1}%)",
            100.0 * working as f64 / counted as f64,
        );
    }
    Ok(ExitCode::SUCCESS)
}
