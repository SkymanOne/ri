//! Turning arguments into a session: settings, trust, session file, model, tools
//! and prompt resources. Mirrors `main.ts` and `sdk.ts` in pi `v1.0.0`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use ri_ai::api::Apis;
use ri_ai::registry::ModelRegistry;
use ri_core::agent_session::{AgentSession, Resources, SessionConfig};
use ri_core::config::{SESSION_DIR_ENV, agent_dir, default_session_dir};
use ri_core::model_resolver::{DEFAULT_THINKING_LEVEL, initial_model, resolve_cli_model};
use ri_core::packages::PackageResources;
use ri_core::resources::{context_files, prompt_templates, skills, system_prompt_file};
use ri_core::session::{self, SessionManager};
use ri_core::settings::Scope;
use ri_core::settings::SettingsManager;
use ri_core::tools::path::{expand, resolve_to_cwd};
use ri_core::trust::{TrustStore, resolve_trusted};
use ri_ext::ExtensionHost;
use ri_types::message::{ImageContent, ThinkingLevel};
use ri_types::rpc::SourceInfo;

use crate::args::{Args, FlagValue};

/// A ready session and what to send to it.
pub struct Startup {
    /// The session.
    pub session: AgentSession,
    /// The first prompt: piped stdin, `@file` contents and the first message.
    pub initial_message: Option<String>,
    /// Images from `@file` arguments.
    pub initial_images: Vec<ImageContent>,
    /// Remaining messages, sent one after another.
    pub messages: Vec<String>,
}

/// Reads piped stdin when stdin is not a terminal; empty input is no input.
pub fn read_piped_stdin() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// pi's `resolvePromptInput`: a path to an existing file means its contents.
fn prompt_input(input: &str) -> String {
    let path = Path::new(input);
    if path.exists()
        && let Ok(text) = std::fs::read_to_string(path)
    {
        return text
            .strip_prefix('\u{feff}')
            .map(str::to_owned)
            .unwrap_or(text);
    }
    input.to_owned()
}

fn file_arguments(files: &[String], cwd: &Path) -> anyhow::Result<(String, Vec<ImageContent>)> {
    let mut text = String::new();
    let mut images = Vec::new();
    for file in files {
        let path = ri_core::tools::path::resolve_read_path(file, cwd);
        let metadata = std::fs::metadata(&path)
            .map_err(|_| anyhow::anyhow!("Error: File not found: {}", path.display()))?;
        if metadata.len() == 0 {
            continue;
        }
        let bytes = std::fs::read(&path)
            .with_context(|| format!("Error: Could not read file {}", path.display()))?;
        if let Some(mime_type) = ri_core::tools::image_mime_type(&bytes) {
            images.push(ImageContent {
                data: ri_core::tools::base64(&bytes),
                mime_type: mime_type.to_owned(),
            });
            text += &format!("<file name=\"{}\"></file>\n", path.display());
        } else {
            let content = String::from_utf8_lossy(&bytes);
            let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
            text += &format!("<file name=\"{}\">\n{content}\n</file>\n", path.display());
        }
    }
    Ok((text, images))
}

/// Where `--session`, `--fork` and `--resume` arguments point.
enum Resolved {
    /// A file path or a session of this project.
    Local(PathBuf),
    /// A session of another project, with its working directory.
    Global(PathBuf, String),
    /// Nothing matched.
    NotFound,
}

/// pi's `resolveSessionPath`: a path, else an exact or prefix id in this
/// project, else an exact or prefix id in any project.
fn resolve_session(
    target: &str,
    cwd: &Path,
    dir: &Path,
    cwd_filter: Option<&Path>,
    custom_dir: Option<&Path>,
    agent_dir: &Path,
) -> Resolved {
    if target.contains('/') || target.contains('\\') || target.ends_with(".jsonl") {
        return Resolved::Local(resolve_to_cwd(target, cwd));
    }
    if let Some(path) = session::find_by_id(dir, target, cwd_filter) {
        return Resolved::Local(path);
    }
    if let Some(found) = session::list(dir, cwd_filter)
        .into_iter()
        .find(|summary| summary.id.starts_with(target))
    {
        return Resolved::Local(found.path);
    }
    let all = match custom_dir {
        Some(custom) => session::list(custom, None),
        None => session::list_all(&agent_dir.join("sessions")),
    };
    match all
        .iter()
        .find(|summary| summary.id == target)
        .or_else(|| all.iter().find(|summary| summary.id.starts_with(target)))
    {
        Some(found) => Resolved::Global(found.path.clone(), found.cwd.clone()),
        None => Resolved::NotFound,
    }
}

/// Asks a yes/no question on the terminal; anything but `y` is no.
fn confirm(question: &str) -> bool {
    use std::io::{BufRead, Write};
    let mut out = std::io::stdout();
    let _ = write!(out, "{question} [y/N] ");
    let _ = out.flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// pi's `createSessionManager`. `custom_dir` is the session directory from
/// `--session-dir`, the environment or settings.
fn open_session(
    args: &Args,
    cwd: &Path,
    agent_dir: &Path,
    custom_dir: Option<&Path>,
) -> anyhow::Result<SessionManager> {
    if args.no_session || args.help || args.list_models.is_some() {
        let mut session = SessionManager::in_memory(cwd);
        if let Some(id) = &args.session_id {
            session.new_session(Some(id.clone()), None);
        }
        return Ok(session);
    }
    let default_dir = default_session_dir(agent_dir, cwd);
    let dir = custom_dir.map_or_else(|| default_dir.clone(), Path::to_path_buf);
    // A shared custom directory holds other projects' sessions too.
    let filter = custom_dir.is_some() && dir != default_dir;
    let cwd_filter = filter.then_some(cwd);
    let fail = |err: ri_core::session::SessionError| anyhow::anyhow!("Error: {err}");

    if let Some(source) = &args.fork {
        if let Some(id) = &args.session_id
            && session::find_by_id(&dir, id, cwd_filter).is_some()
        {
            bail!("Session already exists with id '{id}'");
        }
        return match resolve_session(source, cwd, &dir, cwd_filter, custom_dir, agent_dir) {
            Resolved::Local(path) | Resolved::Global(path, _) => {
                SessionManager::fork_from(&path, cwd, &dir, args.session_id.clone()).map_err(fail)
            }
            Resolved::NotFound => bail!("No session found matching '{source}'"),
        };
    }
    if let Some(target) = &args.session {
        return match resolve_session(target, cwd, &dir, cwd_filter, custom_dir, agent_dir) {
            Resolved::Local(path) => SessionManager::open(&path, custom_dir, None).map_err(fail),
            Resolved::Global(path, other) => {
                if !confirm(&format!(
                    "Session found in different project: {other}\nFork this session into current directory?"
                )) {
                    use std::io::Write;
                    let _ = writeln!(std::io::stdout(), "Aborted.");
                    std::process::exit(0);
                }
                SessionManager::fork_from(&path, cwd, &dir, None).map_err(fail)
            }
            Resolved::NotFound => bail!("No session found matching '{target}'"),
        };
    }
    if args.resume {
        bail!(
            "Selecting a session with --resume needs interactive mode; use --session or --continue"
        );
    }
    if args.continue_ {
        return SessionManager::continue_recent(cwd, &dir, filter).map_err(fail);
    }
    if let Some(id) = &args.session_id {
        if let Some(path) = session::find_by_id(&dir, id, cwd_filter) {
            return SessionManager::open(&path, custom_dir, None).map_err(fail);
        }
        eprintln!(
            "Warning: No project session found with id '{id}'; creating a new session with that id."
        );
    }
    SessionManager::create(cwd, &dir, args.session_id.clone()).map_err(fail)
}

fn tool_names(args: &Args, settings: &SettingsManager) -> Vec<String> {
    if args.no_tools || args.no_builtin_tools {
        return Vec::new();
    }
    let mut names: Vec<String> = ri_core::tools::DEFAULT_TOOLS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    if let Some(configured) = &settings.settings().default_tools {
        for entry in configured {
            if let Some(name) = entry.strip_prefix('+') {
                if !names.iter().any(|existing| existing == name) {
                    names.push(name.to_owned());
                }
            } else if let Some(name) = entry.strip_prefix('-') {
                names.retain(|existing| existing != name);
            } else {
                names = configured
                    .iter()
                    .filter(|entry| !entry.starts_with('+') && !entry.starts_with('-'))
                    .cloned()
                    .collect();
                break;
            }
        }
    }
    if let Some(tools) = &args.tools {
        names = tools.clone();
    }
    if let Some(excluded) = &args.exclude_tools {
        names.retain(|name| !excluded.contains(name));
    }
    names
}

/// The session directory from `--session-dir`, the environment or settings.
fn custom_session_dir(args: &Args, settings: &SettingsManager, cwd: &Path) -> Option<PathBuf> {
    let env_dir = std::env::var(SESSION_DIR_ENV)
        .ok()
        .filter(|dir| !dir.is_empty());
    args.session_dir
        .as_deref()
        .or(env_dir.as_deref())
        .or(settings.settings().session_dir.as_deref())
        .map(|dir| resolve_to_cwd(dir, cwd))
}

/// Settings for `cwd`, with project settings when the project is trusted, and
/// whether it is.
fn load_settings(
    args: &Args,
    cwd: &Path,
    agent_dir: &Path,
) -> anyhow::Result<(SettingsManager, bool)> {
    let trust = TrustStore::new(agent_dir);
    let global_settings = SettingsManager::load(agent_dir, cwd, false)?;
    let trusted = resolve_trusted(
        cwd,
        &trust,
        args.project_trust_override,
        global_settings.settings().default_project_trust,
    );
    Ok((SettingsManager::load(agent_dir, cwd, trusted)?, trusted))
}

/// Where `--resume` looks: the working directory, the custom session
/// directory if any, and the theme setting.
pub fn resume_context(args: &Args) -> anyhow::Result<(PathBuf, Option<PathBuf>, Option<String>)> {
    let cwd = std::env::current_dir().context("reading the working directory")?;
    let (settings, _) = load_settings(args, &cwd, &agent_dir())?;
    let custom = custom_session_dir(args, &settings, &cwd);
    Ok((cwd, custom, settings.settings().theme.clone()))
}

/// Builds the session for a run. Errors are user-facing messages.
/// pi's message when no model can be chosen outside interactive mode.
pub const NO_MODELS_MESSAGE: &str =
    "No models available. Use /login to log into a provider via OAuth or API key.";

pub fn start(
    args: &mut Args,
    stdin: Option<String>,
    extensions: &Extensions,
) -> anyhow::Result<Startup> {
    let cwd = std::env::current_dir().context("reading the working directory")?;
    let agent_dir = agent_dir();
    let (settings, _) = load_settings(args, &cwd, &agent_dir)?;
    let custom_dir = custom_session_dir(args, &settings, &cwd);
    let session = open_session(args, &cwd, &agent_dir, custom_dir.as_deref())?;
    let session = create(args, session, true, extensions)?;

    let (file_text, images) = file_arguments(&args.file_args, &cwd)?;
    let mut parts = Vec::new();
    if let Some(stdin) = stdin {
        parts.push(stdin);
    }
    if !file_text.is_empty() {
        parts.push(file_text);
    }
    if !args.messages.is_empty() {
        parts.push(args.messages.remove(0));
    }
    let initial_message = (!parts.is_empty()).then(|| parts.concat());

    if let Some(name) = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        session.set_name(name);
    }
    Ok(Startup {
        session,
        initial_message,
        initial_images: images,
        messages: std::mem::take(&mut args.messages),
    })
}

/// Builds a session around `session` in its working directory, as pi's
/// runtime factory does for the first session and every one that replaces it:
/// settings, model, thinking level, tools and prompt resources follow the
/// arguments. `warn` reports model problems on stderr, which only the first
/// session may do.
pub fn create(
    args: &Args,
    session: SessionManager,
    warn: bool,
    extensions: &Extensions,
) -> anyhow::Result<AgentSession> {
    let cwd = session.cwd().to_path_buf();
    let agent_dir = agent_dir();
    let (settings, trusted) = load_settings(args, &cwd, &agent_dir)?;
    let mut registry = ModelRegistry::load(&agent_dir);
    if warn && let Some(error) = registry.error() {
        eprintln!("Warning: errors loading models.json:\n{error}");
    }
    let existing = session.entries().next().is_some();

    // Model: --model, then the session's last model, then defaults.
    let mut model = None;
    let mut thinking: Option<ThinkingLevel> = args.thinking;
    if let Some(pattern) = &args.model {
        let resolved =
            resolve_cli_model(args.provider.as_deref(), pattern, args.thinking, &registry);
        if warn && let Some(warning) = &resolved.warning {
            eprintln!("Warning: {warning}");
        }
        if let Some(error) = resolved.error {
            bail!(error);
        }
        if thinking.is_none() {
            thinking = resolved.thinking_level;
        }
        model = resolved.model;
    } else if args.provider.is_some() {
        bail!(
            "--provider requires --model (for example: --provider {} --model <pattern>)",
            args.provider.as_deref().unwrap_or_default()
        );
    }
    let context = session.build_context();
    if model.is_none()
        && existing
        && let Some((provider, id)) = &context.model
    {
        model = registry.find(provider, id).cloned();
        if thinking.is_none() {
            thinking = ThinkingLevel::parse(&context.thinking_level);
        }
    }
    let settings_view = settings.settings();
    if model.is_none() {
        model = initial_model(
            &registry,
            settings_view.default_provider.as_deref(),
            settings_view.default_model.as_deref(),
        );
    }
    if let Some(key) = &args.api_key {
        let Some(model) = &model else {
            bail!(
                "--api-key requires a model to be specified via --model, --provider/--model, or --models"
            );
        };
        registry.set_runtime_key(&model.provider, key.clone());
    }
    let mut thinking_level = thinking
        .or_else(|| {
            let model = model.as_ref()?;
            settings_view
                .model_thinking_levels
                .as_ref()
                .and_then(|levels| levels.get(&model.reference()))
                .copied()
        })
        .or(settings_view.default_thinking_level)
        .unwrap_or(DEFAULT_THINKING_LEVEL);
    if let Some(model) = &model {
        thinking_level = ri_ai::thinking::clamp_level(model, thinking_level);
    }

    let tools = tool_names(args, &settings);
    let extra_skills: Vec<PathBuf> = settings_view
        .skills
        .iter()
        .flatten()
        .chain(&args.skills)
        .map(|path| PathBuf::from(expand(path)))
        .chain(extensions.resources.skills.iter().cloned())
        .collect();
    let extra_templates: Vec<PathBuf> = settings_view
        .prompts
        .iter()
        .flatten()
        .chain(&args.prompt_templates)
        .map(|path| PathBuf::from(expand(path)))
        .chain(extensions.resources.prompts.iter().cloned())
        .collect();
    let mut appends: Vec<String> =
        system_prompt_file(&cwd, &agent_dir, trusted, "APPEND_SYSTEM.md")
            .into_iter()
            .collect();
    appends.extend(
        args.append_system_prompt
            .iter()
            .map(|input| prompt_input(input)),
    );
    let resources = Resources {
        context_files: if args.no_context_files {
            Vec::new()
        } else {
            context_files(&cwd, &agent_dir)
        },
        skills: if args.no_skills {
            Vec::new()
        } else {
            skills(&cwd, &agent_dir, trusted, &extra_skills)
        },
        templates: if args.no_prompt_templates {
            Vec::new()
        } else {
            prompt_templates(&cwd, &agent_dir, trusted, &extra_templates)
        },
        custom_prompt: args
            .system_prompt
            .as_deref()
            .map(prompt_input)
            .or_else(|| system_prompt_file(&cwd, &agent_dir, trusted, "SYSTEM.md")),
        append_prompt: (!appends.is_empty()).then(|| appends.join("\n\n")),
    };

    Ok(AgentSession::new(SessionConfig {
        cwd,
        agent_dir,
        settings,
        registry,
        apis: Apis::default(),
        session,
        model,
        thinking_level,
        tools,
        // pi runs its built-in extensions after the loaded ones.
        extensions: extensions
            .hosts
            .iter()
            .flat_map(ExtensionHost::for_session)
            .chain(ri_core::extensions::builtins())
            .collect(),
        include_extension_tools: args.tools.is_none() && !args.no_tools,
        resources,
    }))
}

/// pi's hint after an extension fails to load.
pub const EXTENSION_LOAD_FAILURE_HINT: &str = "Hint: Start without extensions using \"ri -ne\".";

/// Why the run's extensions cannot start: pi's startup diagnostics.
pub struct ExtensionErrors {
    /// Error messages, without the `Error: ` prefix.
    pub messages: Vec<String>,
    /// Whether an extension failed to load, which earns pi's hint.
    pub load_failed: bool,
}

/// Loads the run's pi extensions: `-e` paths, then, unless `--no-extensions`,
/// those installed in a trusted project and in the agent directory. Applies
/// extension flags from the command line, which must name registered flags.
pub async fn load_extensions(args: &Args) -> Result<Extensions, ExtensionErrors> {
    use ri_core::extensions::discovery;
    let fail = |message: String| ExtensionErrors {
        messages: vec![message],
        load_failed: false,
    };
    let cwd = std::env::current_dir()
        .map_err(|err| fail(format!("reading the working directory: {err}")))?;
    let agent_dir = agent_dir();
    let (settings, trusted) =
        load_settings(args, &cwd, &agent_dir).map_err(|err| fail(err.to_string()))?;
    let mut messages = Vec::new();
    let mut missing = Vec::new();
    let mut requested = Vec::new();
    for path in &args.extensions {
        let resolved = resolve_to_cwd(path, &cwd);
        if resolved.exists() {
            requested.push(path.clone());
        } else {
            missing.push(format!(
                "Failed to load extension \"{0}\": Extension path does not exist: {0}",
                resolved.display()
            ));
        }
    }
    // pi's source info: `cli` for `-e`, `local` for settings entries, `auto`
    // for installed ones, and the package's source for package resources.
    let source = |path: PathBuf, source: &str, scope: &str| SourceInfo {
        path: path.to_string_lossy().into_owned(),
        source: source.into(),
        scope: scope.into(),
        origin: "top-level".into(),
        base_dir: None,
    };
    let scope_name = |scope: Scope| match scope {
        Scope::Global => "user",
        Scope::Project => "project",
    };
    let mut sources: Vec<SourceInfo> = discovery::configured(&requested, &cwd)
        .into_iter()
        .map(|path| source(path, "cli", "temporary"))
        .collect();
    let mut resources = PackageResources::default();
    if !args.no_extensions {
        let mut packages = ri_core::packages::PackageManager::new(
            cwd.clone(),
            agent_dir.clone(),
            settings,
            ri_core::packages::npm::default_registry(),
        );
        let offline = args.offline || ri_core::tools::external::offline();
        let resolved = packages
            .resolve(!offline, |message| eprintln!("Warning: {message}"))
            .await;
        // pi's precedence: project settings and installed extensions, then
        // the user's, then packages.
        let settings_entries = packages.settings_extensions();
        let installed = discovery::installed(&cwd, &agent_dir, trusted);
        let project = cwd.join(ri_core::config::PROJECT_DIR);
        for scope in [Scope::Project, Scope::Global] {
            for (path, _) in settings_entries.iter().filter(|(_, entry)| *entry == scope) {
                sources.push(source(path.clone(), "local", scope_name(scope)));
            }
            for path in &installed {
                if path.starts_with(&project) == (scope == Scope::Project) {
                    sources.push(source(path.clone(), "auto", scope_name(scope)));
                }
            }
        }
        for package in resolved {
            for path in &package.resources.extensions {
                sources.push(SourceInfo {
                    path: path.to_string_lossy().into_owned(),
                    source: package.source.clone(),
                    scope: scope_name(package.scope).into(),
                    origin: "package".into(),
                    base_dir: Some(package.root.to_string_lossy().into_owned()),
                });
            }
            resources.skills.extend(package.resources.skills);
            resources.prompts.extend(package.resources.prompts);
            resources.themes.extend(package.resources.themes);
        }
    }
    let mut seen = std::collections::HashSet::new();
    sources.retain(|source| seen.insert(source.path.clone()));
    // pi extensions share one JS runtime; each native extension has its own.
    let mut hosts = Vec::new();
    if !sources.is_empty() {
        let cache = agent_dir.join("cache");
        let engine =
            ri_ext::Engine::new(Some(&cache.join("wasm"))).map_err(|err| fail(err.to_string()))?;
        let mut options = ri_ext::Options::new(cwd.clone());
        options.agent_dir = agent_dir.clone();
        options.cache_dir = Some(cache.join("js"));
        let (native, js): (Vec<SourceInfo>, Vec<SourceInfo>) = sources
            .into_iter()
            .partition(|source| source.path.ends_with(".wasm"));
        if !js.is_empty() {
            let host = ExtensionHost::load(&engine, options.clone(), &js)
                .await
                .map_err(|err| fail(err.to_string()))?;
            hosts.push(host);
        }
        for source in &native {
            match ExtensionHost::load_native(&engine, options.clone(), source).await {
                Ok(host) => hosts.push(host),
                Err(err) => messages.push(format!(
                    "Failed to load extension \"{}\": {err}",
                    source.path
                )),
            }
        }
        for host in &hosts {
            for error in host.errors() {
                messages.push(format!(
                    "Failed to load extension \"{}\": {}",
                    error.path.display(),
                    error.error
                ));
            }
        }
    }
    messages.append(&mut missing);
    let load_failed = !messages.is_empty();

    // pi's applyExtensionFlagValues.
    let flags: Vec<ri_ext::Flag> = hosts.iter().flat_map(|host| host.flags()).collect();
    let mut values = serde_json::Map::new();
    let mut unknown = Vec::new();
    for (name, value) in &args.unknown_flags {
        let Some(flag) = flags.iter().find(|flag| flag.name == *name) else {
            unknown.push(format!("--{name}"));
            continue;
        };
        match (flag.takes_value, value) {
            (false, _) => {
                values.insert(name.clone(), serde_json::Value::Bool(true));
            }
            (true, FlagValue::Value(text)) => {
                values.insert(name.clone(), serde_json::Value::String(text.clone()));
            }
            (true, FlagValue::Present) => {
                messages.push(format!("Extension flag \"--{name}\" requires a value"));
            }
        }
    }
    if !unknown.is_empty() {
        let plural = if unknown.len() == 1 { "" } else { "s" };
        messages.push(format!("Unknown option{plural}: {}", unknown.join(", ")));
    }
    if !messages.is_empty() {
        return Err(ExtensionErrors {
            messages,
            load_failed,
        });
    }
    if !values.is_empty() {
        for host in &hosts {
            host.set_flags(values.clone())
                .await
                .map_err(|err| fail(err.to_string()))?;
        }
    }
    Ok(Extensions { hosts, resources })
}

/// The run's loaded extensions, and the skills, prompt templates and themes
/// its packages provide.
#[derive(Default)]
pub struct Extensions {
    /// Instances with loaded extensions.
    pub hosts: Vec<Arc<ExtensionHost>>,
    /// Package resources besides extensions.
    pub resources: PackageResources,
}
