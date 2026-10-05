//! Command-line arguments, parsed as pi parses them.
//!
//! Port of `packages/coding-agent/src/cli/args.ts` in pi `v1.0.0`. Unknown
//! `--flags` are kept for extensions instead of being rejected.

use indexmap::IndexMap;
use yapi_types::message::ThinkingLevel;

/// Output mode for non-interactive runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Final assistant text on stdout.
    Text,
    /// One JSON event per line.
    Json,
    /// JSON-RPC over stdin and stdout.
    Rpc,
}

/// A value of an unknown flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagValue {
    /// A bare flag.
    Present,
    /// A flag with a value.
    Value(String),
}

/// A problem with the arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Errors stop the run; warnings do not.
    pub error: bool,
    /// What is wrong.
    pub message: String,
}

/// Parsed arguments.
#[derive(Clone, Debug, Default)]
#[allow(missing_docs, reason = "fields are the flags of the same name")]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<ThinkingLevel>,
    pub continue_: bool,
    pub resume: bool,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub extensions: Vec<String>,
    pub no_extensions: bool,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Vec<String>,
    pub prompt_templates: Vec<String>,
    pub no_prompt_templates: bool,
    pub themes: Vec<String>,
    pub use_theme: Option<String>,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub list_models: Option<Option<String>>,
    pub offline: bool,
    pub tui_mode: Option<String>,
    pub verbose: bool,
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    pub unknown_flags: IndexMap<String, FlagValue>,
    pub diagnostics: Vec<Diagnostic>,
}

fn list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses arguments after the program name.
pub fn parse(args: &[String]) -> Args {
    let mut result = Args::default();
    let mut index = 0;
    let error = |result: &mut Args, message: String| {
        result.diagnostics.push(Diagnostic {
            error: true,
            message,
        });
    };
    while index < args.len() {
        let arg = args[index].as_str();
        let next = args.get(index + 1).map(String::as_str);
        let has_next = next.is_some();
        let mut take = || {
            index += 1;
            args[index].clone()
        };
        match arg {
            "--" => {
                for positional in &args[index + 1..] {
                    match positional.strip_prefix('@') {
                        Some(file) => result.file_args.push(file.to_owned()),
                        None => result.messages.push(positional.clone()),
                    }
                }
                break;
            }
            "--help" | "-h" => result.help = true,
            "--version" | "-v" => result.version = true,
            "--mode" => match next {
                None => error(&mut result, "--mode requires text, json, or rpc".into()),
                Some(mode) if mode.starts_with('-') => {
                    error(&mut result, "--mode requires text, json, or rpc".into())
                }
                Some(mode) => {
                    index += 1;
                    match mode {
                        "text" => result.mode = Some(Mode::Text),
                        "json" => result.mode = Some(Mode::Json),
                        "rpc" => result.mode = Some(Mode::Rpc),
                        other => error(
                            &mut result,
                            format!("Invalid mode \"{other}\". Valid values: text, json, rpc"),
                        ),
                    }
                }
            },
            "--continue" | "-c" => result.continue_ = true,
            "--resume" | "-r" => result.resume = true,
            "--provider" if has_next => result.provider = Some(take()),
            "--model" if has_next => result.model = Some(take()),
            "--api-key" if has_next => result.api_key = Some(take()),
            "--system-prompt" if has_next => result.system_prompt = Some(take()),
            "--append-system-prompt" if has_next => {
                let value = take();
                result.append_system_prompt.push(value);
            }
            "--name" | "-n" => {
                if has_next {
                    result.name = Some(take());
                } else {
                    error(&mut result, "--name requires a value".into());
                }
            }
            "--no-session" => result.no_session = true,
            "--session" if has_next => result.session = Some(take()),
            "--session-id" if has_next => result.session_id = Some(take()),
            "--fork" if has_next => result.fork = Some(take()),
            "--session-dir" if has_next => result.session_dir = Some(take()),
            "--models" if has_next => {
                let value = take();
                result.models = Some(value.split(',').map(|s| s.trim().to_owned()).collect());
            }
            "--no-tools" | "-nt" => result.no_tools = true,
            "--no-builtin-tools" | "-nbt" => result.no_builtin_tools = true,
            "--tools" | "-t" if has_next => {
                let value = take();
                result.tools = Some(list(&value));
            }
            "--exclude-tools" | "-xt" if has_next => {
                let value = take();
                result.exclude_tools = Some(list(&value));
            }
            "--thinking" if has_next => {
                let level = take();
                match ThinkingLevel::parse(&level) {
                    Some(level) => result.thinking = Some(level),
                    None => result.diagnostics.push(Diagnostic {
                        error: false,
                        message: format!(
                            "Invalid thinking level \"{level}\". Valid values: off, minimal, low, medium, high, xhigh, max"
                        ),
                    }),
                }
            }
            "--print" | "-p" => {
                result.print = true;
                if let Some(next) = next
                    && !next.starts_with('@')
                    && (!next.starts_with('-') || next.starts_with("---"))
                {
                    result.messages.push(next.to_owned());
                    index += 1;
                }
            }
            "--export" if has_next => result.export = Some(take()),
            "--extension" | "-e" if has_next => {
                let value = take();
                result.extensions.push(value);
            }
            "--no-extensions" | "-ne" => result.no_extensions = true,
            "--skill" if has_next => {
                let value = take();
                result.skills.push(value);
            }
            "--prompt-template" if has_next => {
                let value = take();
                result.prompt_templates.push(value);
            }
            "--theme" if has_next => {
                let value = take();
                result.themes.push(value);
            }
            "--use-theme" => match next {
                Some(name) if !name.starts_with('-') => {
                    result.use_theme = Some(name.to_owned());
                    index += 1;
                }
                _ => error(&mut result, "--use-theme requires a theme name".into()),
            },
            "--no-skills" | "-ns" => result.no_skills = true,
            "--no-prompt-templates" | "-np" => result.no_prompt_templates = true,
            "--no-themes" => result.no_themes = true,
            "--no-context-files" | "-nc" => result.no_context_files = true,
            "--list-models" => match next {
                Some(search) if !search.starts_with('-') && !search.starts_with('@') => {
                    result.list_models = Some(Some(search.to_owned()));
                    index += 1;
                }
                _ => result.list_models = Some(None),
            },
            "--tui-mode" => match next {
                Some(mode @ ("regular" | "fullscreen")) => {
                    result.tui_mode = Some(mode.to_owned());
                    index += 1;
                }
                Some(mode) if !mode.starts_with('-') => {
                    index += 1;
                    error(
                        &mut result,
                        format!("Invalid TUI mode \"{mode}\". Valid values: regular, fullscreen"),
                    );
                }
                _ => error(
                    &mut result,
                    "--tui-mode requires regular or fullscreen".into(),
                ),
            },
            "--verbose" => result.verbose = true,
            "--approve" | "-a" => result.project_trust_override = Some(true),
            "--no-approve" | "-na" => result.project_trust_override = Some(false),
            "--offline" => result.offline = true,
            _ if arg.starts_with('@') => result.file_args.push(arg[1..].to_owned()),
            _ if arg.starts_with("--") => {
                let flag = &arg[2..];
                match flag.split_once('=') {
                    Some((name, value)) => {
                        result
                            .unknown_flags
                            .insert(name.to_owned(), FlagValue::Value(value.to_owned()));
                    }
                    None => match next {
                        Some(value) if !value.starts_with('-') && !value.starts_with('@') => {
                            result
                                .unknown_flags
                                .insert(flag.to_owned(), FlagValue::Value(value.to_owned()));
                            index += 1;
                        }
                        _ => {
                            result
                                .unknown_flags
                                .insert(flag.to_owned(), FlagValue::Present);
                        }
                    },
                }
            }
            _ if arg.starts_with('-') => error(&mut result, format!("Unknown option: {arg}")),
            _ => result.messages.push(arg.to_owned()),
        }
        index += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &[&str]) -> Args {
        parse(&text.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_like_pi() {
        let parsed = args(&[
            "-p",
            "hello",
            "--model",
            "a/b",
            "@f.txt",
            "--custom",
            "x",
            "--flag",
            "world",
            "--thinking",
            "high",
        ]);
        assert!(parsed.print);
        // An unknown flag takes the next plain argument as its value.
        assert_eq!(parsed.messages, ["hello"]);
        assert_eq!(parsed.model.as_deref(), Some("a/b"));
        assert_eq!(parsed.file_args, ["f.txt"]);
        assert_eq!(parsed.unknown_flags["custom"], FlagValue::Value("x".into()));
        assert_eq!(
            parsed.unknown_flags["flag"],
            FlagValue::Value("world".into())
        );
        assert_eq!(parsed.thinking, Some(ThinkingLevel::High));

        let parsed = args(&["--mode", "json", "-x", "--list-models"]);
        assert_eq!(parsed.mode, Some(Mode::Json));
        assert_eq!(parsed.list_models, Some(None));
        assert_eq!(parsed.diagnostics[0].message, "Unknown option: -x");

        let parsed = args(&["-p", "--model", "m", "--", "@a", "-b"]);
        assert!(parsed.messages == ["-b"] && parsed.file_args == ["a"]);
    }
}
