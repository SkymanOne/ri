//! Turning arguments into a session: settings, trust, session file, model, tools
//! and prompt resources. Mirrors `main.ts` and `sdk.ts` in pi `v1.0.0`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use ri_ai::api::Apis;
use ri_ai::registry::ModelRegistry;
use ri_core::agent_session::{AgentSession, Resources, SessionConfig};
use ri_core::config::{agent_dir, default_session_dir};
use ri_core::model_resolver::{DEFAULT_THINKING_LEVEL, initial_model, resolve_cli_model};
use ri_core::resources::{context_files, prompt_templates, skills, system_prompt_file};
use ri_core::session::{SessionManager, find_by_id};
use ri_core::settings::SettingsManager;
use ri_core::tools::path::expand;
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

fn open_session(args: &Args, cwd: &Path, sessions_dir: &Path) -> anyhow::Result<SessionManager> {
    if args.no_session || args.help || args.list_models.is_some() {
        let mut session = SessionManager::in_memory(cwd);
        if let Some(id) = &args.session_id {
            session.new_session(Some(id.clone()), None);
        }
        return Ok(session);
    }
    let resolve = |target: &str| -> anyhow::Result<PathBuf> {
        let path = PathBuf::from(expand(target));
        if target.contains('/') || target.ends_with(".jsonl") || path.exists() {
            return Ok(if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            });
        }
        find_by_id(sessions_dir, target)?
            .ok_or_else(|| anyhow::anyhow!("No session found matching '{target}'"))
    };
    if let Some(source) = &args.fork {
        let source = resolve(source)?;
        return Ok(SessionManager::fork_from(&source, cwd, sessions_dir)?);
    }
    if let Some(target) = &args.session {
        return Ok(SessionManager::open(&resolve(target)?, None)?);
    }
    if args.continue_ || args.resume {
        return Ok(SessionManager::continue_recent(cwd, sessions_dir)?);
    }
    let mut session = SessionManager::create(cwd, sessions_dir)?;
    if let Some(id) = &args.session_id {
        session.new_session(Some(id.clone()), None);
    }
    Ok(session)
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

/// Builds the session for a run. Errors are user-facing messages.
pub fn start(args: &mut Args, stdin: Option<String>) -> anyhow::Result<Startup> {
    let cwd = std::env::current_dir().context("reading the working directory")?;
    let agent_dir = agent_dir();
    let trust = TrustStore::new(&agent_dir);
    let global_settings = SettingsManager::load(&agent_dir, &cwd, false)?;
    let trusted = resolve_trusted(
        &cwd,
        &trust,
        args.project_trust_override,
        global_settings.settings().default_project_trust,
    );
    let settings = SettingsManager::load(&agent_dir, &cwd, trusted)?;
    let mut registry = ModelRegistry::load(&agent_dir);
    if let Some(error) = registry.error() {
        eprintln!("Warning: errors loading models.json:\n{error}");
    }

    let sessions_dir = match args
        .session_dir
        .as_deref()
        .or(settings.settings().session_dir.as_deref())
    {
        Some(dir) => PathBuf::from(expand(dir)),
        None => default_session_dir(&agent_dir, &cwd),
    };
    let session = open_session(args, &cwd, &sessions_dir)?;
    let existing = session.entries().next().is_some();

    // Model: --model, then the session's last model, then defaults.
    let mut model = None;
    let mut thinking: Option<ThinkingLevel> = args.thinking;
    if let Some(pattern) = &args.model {
        let resolved =
            resolve_cli_model(args.provider.as_deref(), pattern, args.thinking, &registry);
        if let Some(warning) = &resolved.warning {
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

    let session = AgentSession::new(SessionConfig {
        cwd,
        agent_dir,
        settings,
        registry,
        apis: Apis::default(),
        session,
        model,
        thinking_level,
        tools,
        resources,
    });
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
