//! Turning arguments into a session: settings, trust, session file, model, tools
//! and prompt resources. Mirrors `main.ts` and `sdk.ts` in pi `v1.0.0`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use ri_ai::api::Apis;
use ri_ai::registry::ModelRegistry;
use ri_core::agent_session::{AgentSession, Resources, SessionConfig};
use ri_core::config::{SESSION_DIR_ENV, agent_dir, default_session_dir};
use ri_core::model_resolver::{DEFAULT_THINKING_LEVEL, initial_model, resolve_cli_model};
use ri_core::resources::{context_files, prompt_templates, skills, system_prompt_file};
use ri_core::session::{self, SessionManager};
use ri_core::settings::SettingsManager;
use ri_core::tools::path::{expand, resolve_to_cwd};
use ri_core::trust::{TrustStore, resolve_trusted};
use ri_types::message::{ImageContent, ThinkingLevel};

use crate::args::Args;

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

pub fn start(args: &mut Args, stdin: Option<String>) -> anyhow::Result<Startup> {
    let cwd = std::env::current_dir().context("reading the working directory")?;
    let agent_dir = agent_dir();
    let (settings, _) = load_settings(args, &cwd, &agent_dir)?;
    let custom_dir = custom_session_dir(args, &settings, &cwd);
    let session = open_session(args, &cwd, &agent_dir, custom_dir.as_deref())?;
    let session = create(args, session, true)?;

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
pub fn create(args: &Args, session: SessionManager, warn: bool) -> anyhow::Result<AgentSession> {
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
        .collect();
    let extra_templates: Vec<PathBuf> = settings_view
        .prompts
        .iter()
        .flatten()
        .chain(&args.prompt_templates)
        .map(|path| PathBuf::from(expand(path)))
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
        extensions: ri_core::extensions::builtins(),
        resources,
    }))
}
