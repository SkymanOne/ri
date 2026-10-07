//! Context files, skills, prompt templates and system prompt files.
//!
//! Ports of the discovery parts of `resource-loader.ts`, `skills.ts` and
//! `prompt-templates.ts` in pi `v1.0.0`. Project `.yapi/` resources load only for a
//! trusted project; `AGENTS.md` and `CLAUDE.md` files always load.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex_lite::Regex;
use serde_json::Value;
pub use yapi_types::rpc::SourceInfo;
pub use yapi_types::system_prompt::{ContextFile, Skill};

use crate::config::PROJECT_DIR;

/// A problem found while loading skills, prompt templates or themes: pi's
/// `ResourceDiagnostic`, listed under the startup `[... conflicts]` sections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    /// A resource that loads with a problem, or a path that does not load.
    Warning {
        /// What is wrong.
        message: String,
        /// The path.
        path: PathBuf,
    },
    /// A command-line path that does not exist.
    Error {
        /// What is wrong.
        message: String,
        /// The path.
        path: PathBuf,
    },
    /// A name an earlier resource took; the later one is skipped.
    Collision {
        /// The name.
        name: String,
        /// The resource that keeps the name, with its source.
        winner: SourceInfo,
        /// The skipped file.
        loser: PathBuf,
    },
}

/// A prompt template, expanded by `/name args`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplate {
    /// The command name: the file name without `.md`.
    pub name: String,
    /// From frontmatter, else the first non-empty line, cut at 60 characters.
    pub description: String,
    /// `argument-hint` from frontmatter.
    pub argument_hint: Option<String>,
    /// The body.
    pub content: String,
    /// The file.
    pub file_path: PathBuf,
    /// Where it was found.
    pub source: SourceInfo,
}

/// Frontmatter as pi's `yaml` parser gives it to JavaScript: the top-level
/// mapping, in document order.
pub type Frontmatter = serde_json::Map<String, Value>;

/// pi's `extractFrontmatter`: the YAML between `---` lines, when there is
/// any, and the trimmed markdown body after it.
pub fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let normalized = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    match normalized
        .starts_with("---")
        .then(|| normalized[3..].find("\n---"))
        .flatten()
    {
        Some(end) => (
            Some(normalized.get(4..end + 3).unwrap_or_default().to_owned()),
            normalized[end + 7..].trim().to_owned(),
        ),
        None => (None, normalized),
    }
}

/// pi's `parseFrontmatter`: the frontmatter and the body, or the YAML error.
pub fn parse_frontmatter(content: &str) -> Result<(Frontmatter, String), String> {
    let (yaml, body) = split_frontmatter(content);
    let Some(yaml) = yaml.filter(|yaml| !yaml.is_empty()) else {
        return Ok((Frontmatter::new(), body));
    };
    let documents =
        yaml_rust2::YamlLoader::load_from_str(&yaml).map_err(|error| error.to_string())?;
    let frontmatter = match documents.into_iter().next().map(yaml_to_json) {
        Some(Value::Object(map)) => map,
        _ => Frontmatter::new(),
    };
    Ok((frontmatter, body))
}

/// A YAML value as JavaScript sees it; keys become strings.
fn yaml_to_json(yaml: yaml_rust2::Yaml) -> Value {
    use yaml_rust2::Yaml;
    match yaml {
        Yaml::Real(_) => yaml
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::Null, Value::Number),
        Yaml::Integer(number) => Value::from(number),
        Yaml::String(text) => Value::String(text),
        Yaml::Boolean(flag) => Value::Bool(flag),
        Yaml::Array(items) => Value::Array(items.into_iter().map(yaml_to_json).collect()),
        Yaml::Hash(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| {
                    let key = match yaml_to_json(key) {
                        Value::String(text) => text,
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    (key, yaml_to_json(value))
                })
                .collect(),
        ),
        Yaml::Null | Yaml::BadValue | Yaml::Alias(_) => Value::Null,
    }
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|text| {
        text.strip_prefix('\u{feff}')
            .map(str::to_owned)
            .unwrap_or(text)
    })
}

fn context_file_in(dir: &Path) -> Option<ContextFile> {
    for name in [
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
    ] {
        let path = dir.join(name);
        if path.is_file()
            && let Some(content) = read_text(&path)
        {
            return Some(ContextFile { path, content });
        }
    }
    None
}

/// The global context file from the agent directory, then those of `cwd` and its
/// ancestors, outermost first.
pub fn context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    if let Some(global) = context_file_in(agent_dir) {
        files.push(global);
    }
    let mut ancestors = Vec::new();
    for dir in cwd.ancestors() {
        if let Some(file) = context_file_in(dir)
            && !files
                .iter()
                .any(|seen: &ContextFile| seen.path == file.path)
            && !ancestors
                .iter()
                .any(|seen: &ContextFile| seen.path == file.path)
        {
            ancestors.insert(0, file);
        }
    }
    files.extend(ancestors);
    files
}

/// The source of a resource at `path` found under `origin`, whose path it
/// replaces.
fn found_at(origin: &SourceInfo, path: &Path) -> SourceInfo {
    SourceInfo {
        path: path.display().to_string(),
        ..origin.clone()
    }
}

/// pi's collision rule for resources in precedence order: the first of each
/// name is kept and later ones are reported as collisions. `meta` gives a
/// resource's name, source and file. A resource whose `real_path` a kept one
/// has is the same file reached again and is skipped silently.
fn first_by_name<T>(
    found: Vec<T>,
    meta: impl Fn(&T) -> (&str, &SourceInfo, &Path),
    real_path: impl Fn(&T) -> Option<PathBuf>,
) -> (Vec<T>, Vec<Diagnostic>) {
    let mut kept: Vec<T> = Vec::new();
    let mut files = HashSet::new();
    let mut collisions = Vec::new();
    for item in found {
        let real = real_path(&item);
        if real.as_ref().is_some_and(|real| files.contains(real)) {
            continue;
        }
        let (name, _, file) = meta(&item);
        match kept.iter().find(|existing| meta(existing).0 == name) {
            Some(existing) => collisions.push(Diagnostic::Collision {
                name: name.to_owned(),
                winner: meta(existing).1.clone(),
                loser: file.to_path_buf(),
            }),
            None => {
                files.extend(real);
                kept.push(item);
            }
        }
    }
    (kept, collisions)
}

/// A path given on the command line, as pi records it: source `cli`, scope
/// `temporary`.
pub fn cli_source(path: &Path) -> SourceInfo {
    SourceInfo {
        path: path.display().to_string(),
        source: "cli".into(),
        scope: "temporary".into(),
        origin: "top-level".into(),
        base_dir: None,
    }
}

/// The Agent Skills limits pi checks names and descriptions against, in
/// UTF-16 code units as JavaScript counts them.
const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// pi's `validateName`.
fn name_problems(name: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let length = yapi_types::js::len(name);
    if length > MAX_NAME_LENGTH {
        problems.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({length})"
        ));
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        problems.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        problems.push("name must not start or end with a hyphen".to_owned());
    }
    if name.contains("--") {
        problems.push("name must not contain consecutive hyphens".to_owned());
    }
    problems
}

/// pi's `loadSkillFromFile`: a `SKILL.md` file, or another markdown file with
/// a description. Problems with the name or description are reported; a
/// skill without a description does not load.
fn load_skill(
    path: &Path,
    origin: &SourceInfo,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Skill> {
    let warn = |message: String| Diagnostic::Warning {
        message,
        path: path.to_path_buf(),
    };
    let declared = path.file_name().is_some_and(|name| name == "SKILL.md");
    // The frontmatter parser skips a byte order mark.
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            diagnostics.push(warn(crate::tools::node_error(&error, "open", path)));
            return None;
        }
    };
    let frontmatter = match parse_frontmatter(&text) {
        Ok((frontmatter, _)) => frontmatter,
        Err(error) => {
            if declared {
                diagnostics.push(warn(error));
            }
            return None;
        }
    };
    let description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty());
    if !declared && description.is_none() {
        return None;
    }
    match description {
        None => diagnostics.push(warn("description is required".to_owned())),
        Some(text) => {
            let length = yapi_types::js::len(text);
            if length > MAX_DESCRIPTION_LENGTH {
                diagnostics.push(warn(format!(
                    "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({length})"
                )));
            }
        }
    }
    let base_dir = path.parent()?.to_path_buf();
    let name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            base_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })?;
    diagnostics.extend(name_problems(&name).into_iter().map(warn));
    Some(Skill {
        name,
        description: description?.to_owned(),
        file_path: path.to_path_buf(),
        base_dir,
        disable_model_invocation: frontmatter.get("disable-model-invocation")
            == Some(&Value::Bool(true)),
        source: found_at(origin, path),
    })
}

/// Skills under `dir`: a directory with `SKILL.md` is one skill; otherwise
/// subdirectories are searched, and at the top level other `.md` files with a
/// description are skills too. Hidden entries and `node_modules` are skipped.
fn skills_in(
    dir: &Path,
    top: bool,
    origin: &SourceInfo,
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let skill_file = dir.join("SKILL.md");
    if skill_file.is_file() {
        skills.extend(load_skill(&skill_file, origin, diagnostics));
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = read.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            skills_in(&path, false, origin, skills, diagnostics);
        } else if top && path.is_file() && name.ends_with(".md") {
            skills.extend(load_skill(&path, origin, diagnostics));
        }
    }
}

/// pi's `loadSkills` over `sources`, in order, each with its source: skill
/// files, or directories searched for skills. The first skill of a name
/// wins; later ones are reported as collisions.
pub fn skills_from(sources: &[SourceInfo]) -> (Vec<Skill>, Vec<Diagnostic>) {
    let mut found = Vec::new();
    let mut diagnostics = Vec::new();
    for source in sources {
        let path = Path::new(&source.path);
        let warn = |message: &str| Diagnostic::Warning {
            message: message.to_owned(),
            path: path.to_path_buf(),
        };
        if !path.exists() {
            diagnostics.push(warn("skill path does not exist"));
        } else if path.is_dir() {
            skills_in(path, true, source, &mut found, &mut diagnostics);
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            found.extend(load_skill(path, source, &mut diagnostics));
        } else {
            diagnostics.push(warn("skill path is not a markdown file"));
        }
    }
    // The same file reached twice, through a symlink, loads once.
    let (skills, collisions) = first_by_name(
        found,
        |skill| (&skill.name, &skill.source, &skill.file_path),
        |skill| Some(std::fs::canonicalize(&skill.file_path).unwrap_or(skill.file_path.clone())),
    );
    diagnostics.extend(collisions);
    (skills, diagnostics)
}

/// pi's `mergePaths`: `sources` in order, each path once, compared after
/// resolving symlinks.
pub fn merge_sources(sources: impl IntoIterator<Item = SourceInfo>) -> Vec<SourceInfo> {
    let mut seen = HashSet::new();
    sources
        .into_iter()
        .filter(|info| {
            seen.insert(
                std::fs::canonicalize(&info.path).unwrap_or_else(|_| PathBuf::from(&info.path)),
            )
        })
        .collect()
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The skills section of the prompt, read with `read` or `bash`.
pub fn format_skills(skills: &[Skill], read_tool: &str) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_owned(),
        if read_tool == "read" {
            "Use the read tool to load a skill's file when the task matches its description."
        } else {
            "Use bash to load a skill's file when the task matches its description."
        }
        .to_owned(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];
    for skill in visible {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.to_string_lossy())
        ));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n")
}

fn templates_in(
    dir: &Path,
    origin: &SourceInfo,
    templates: &mut Vec<PromptTemplate>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = read.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            templates.extend(template_at(path, origin, diagnostics));
        }
    }
}

/// pi's `loadTemplateFromFile`: the template in a markdown file. A file that
/// cannot be read or whose frontmatter does not parse is reported.
fn template_at(
    path: PathBuf,
    origin: &SourceInfo,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<PromptTemplate> {
    let parsed = std::fs::read_to_string(&path)
        .map_err(|error| crate::tools::node_error(&error, "open", &path))
        .and_then(|text| parse_frontmatter(&text));
    let (frontmatter, body) = match parsed {
        Ok(parsed) => parsed,
        Err(message) => {
            diagnostics.push(Diagnostic::Warning { message, path });
            return None;
        }
    };
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if description.is_empty()
        && let Some(first) = body.lines().find(|line| !line.trim().is_empty())
    {
        description = yapi_types::js::slice(first, 0, 60);
        if yapi_types::js::len(first) > 60 {
            description += "...";
        }
    }
    Some(PromptTemplate {
        name,
        description,
        argument_hint: frontmatter
            .get("argument-hint")
            .and_then(Value::as_str)
            .map(str::to_owned),
        content: body,
        source: found_at(origin, &path),
        file_path: path,
    })
}

/// pi's prompt template loading over `sources`, in order, each with its
/// source: markdown files, or directories of them. The first template of a
/// name wins; later ones are reported as collisions. Missing paths are
/// skipped; the command line's are reported by the caller.
pub fn templates_from(sources: &[SourceInfo]) -> (Vec<PromptTemplate>, Vec<Diagnostic>) {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    let mut diagnostics = Vec::new();
    for source in sources {
        let path = PathBuf::from(&source.path);
        if path.is_dir() {
            templates_in(&path, source, &mut templates, &mut diagnostics);
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            templates.extend(template_at(path, source, &mut diagnostics));
        }
    }
    let (templates, collisions) = first_by_name(
        templates,
        |template| (&template.name, &template.source, &template.file_path),
        |_| None,
    );
    diagnostics.extend(collisions);
    (templates, diagnostics)
}

/// Splits arguments on whitespace, honoring single and double quotes.
pub fn parse_command_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// pi's `substituteArgs`: replaces `$1`, `$@`, `$ARGUMENTS`, `${N:-default}`
/// and `${@:start:length}`.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    static PLACEHOLDER: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)")
            .ok()
    });
    let Some(placeholder) = PLACEHOLDER.as_ref() else {
        return content.to_owned();
    };
    let all = args.join(" ");
    // Digits too many for an index name no argument, as in JavaScript.
    let number = |digits: &str| digits.parse::<usize>().unwrap_or(usize::MAX);
    let arg = |digits: &str| {
        number(digits)
            .checked_sub(1)
            .and_then(|index| args.get(index))
            .map_or("", String::as_str)
    };
    let all_or_arg = |target: &str| match target {
        "@" | "ARGUMENTS" => all.as_str(),
        digits => arg(digits),
    };
    placeholder
        .replace_all(content, |caps: &regex_lite::Captures<'_>| {
            if let Some(target) = caps.get(1) {
                let value = all_or_arg(target.as_str());
                let default = caps.get(2).map_or("", |default| default.as_str());
                return if value.is_empty() { default } else { value }.to_owned();
            }
            if let Some(start) = caps.get(3) {
                let start = number(start.as_str()).saturating_sub(1).min(args.len());
                let end = caps.get(4).map_or(args.len(), |length| {
                    start
                        .saturating_add(number(length.as_str()))
                        .min(args.len())
                });
                return args[start..end].join(" ");
            }
            caps.get(5)
                .map_or("", |simple| all_or_arg(simple.as_str()))
                .to_owned()
        })
        .into_owned()
}

/// Expands `/name args` when `name` is a template; other text is unchanged.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    let Some(rest) = text.strip_prefix('/') else {
        return text.to_owned();
    };
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(index) => (&rest[..index], rest[index..].trim_start()),
        None => (rest, ""),
    };
    if name.is_empty() {
        return text.to_owned();
    }
    match templates.iter().find(|template| template.name == name) {
        Some(template) => substitute_args(&template.content, &parse_command_args(args)),
        None => text.to_owned(),
    }
}

/// `SYSTEM.md` replacing the default prompt: the trusted project's, else the
/// global one.
pub fn system_prompt_file(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    name: &str,
) -> Option<String> {
    let project = cwd.join(PROJECT_DIR).join(name);
    if project_trusted && project.exists() {
        return read_text(&project);
    }
    read_text(&agent_dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let (meta, body) = parse_frontmatter(
            "---\nname: demo\ndescription: \"Does: things\"\ndisable-model-invocation: true\nnotes: |\n  a\n    b\nwhen: >-\n  folded\n  text\ntags: [a, b]\nlimit: 3\n---\n\nBody\n",
        )
        .unwrap();
        assert_eq!(
            serde_json::Value::Object(meta),
            serde_json::json!({
                "name": "demo",
                "description": "Does: things",
                "disable-model-invocation": true,
                "notes": "a\n  b\n",
                "when": "folded text",
                "tags": ["a", "b"],
                "limit": 3,
            })
        );
        assert_eq!(body, "Body");
        // pi's `yaml` throws on malformed YAML, which skips the skill.
        assert!(parse_frontmatter("---\nname: a: b\n---\n").is_err());
        assert!(parse_frontmatter("no frontmatter").unwrap().0.is_empty());
    }

    #[test]
    fn reports_malformed_frontmatter_like_pi() {
        let dir = std::env::temp_dir().join(format!("yapi-frontmatter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bad = "---\ndescription: a: b\n---\nBody\n";
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::create_dir_all(dir.join("skills/broken")).unwrap();
        std::fs::write(dir.join("prompts/bad.md"), bad).unwrap();
        std::fs::write(dir.join("prompts/good.md"), "Hello\n").unwrap();
        std::fs::write(dir.join("skills/broken/SKILL.md"), bad).unwrap();
        std::fs::write(dir.join("skills/loose.md"), bad).unwrap();

        // pi's `loadTemplateFromFile` warns and skips the template.
        let (templates, diagnostics) = templates_from(&[cli_source(&dir.join("prompts"))]);
        assert_eq!(templates.len(), 1);
        assert!(matches!(
            diagnostics.as_slice(),
            [Diagnostic::Warning { path, .. }] if path.ends_with("prompts/bad.md")
        ));

        // pi's `loadSkillFromFile` warns only for a declared `SKILL.md`.
        let (skills, diagnostics) = skills_from(&[cli_source(&dir.join("skills"))]);
        assert!(skills.is_empty());
        assert!(matches!(
            diagnostics.as_slice(),
            [Diagnostic::Warning { path, .. }] if path.ends_with("broken/SKILL.md")
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn substitutes_arguments_like_pi() {
        let args: Vec<String> = parse_command_args("one \"two three\" four");
        assert_eq!(args, ["one", "two three", "four"]);
        assert_eq!(
            substitute_args("$1|$2|$9|$@", &args),
            "one|two three||one two three four"
        );
        assert_eq!(substitute_args("${4:-none} ${1:-x}", &args), "none one");
        assert_eq!(
            substitute_args("${@:2} ${@:1:1} $ARGUMENTS", &args),
            "two three four one one two three four"
        );
        assert_eq!(substitute_args("cost $ and $x", &args), "cost $ and $x");
    }

    /// Expectations from pi's `substituteArgs` in Node.
    #[test]
    fn substitutes_edge_cases_like_pi() {
        let args = |list: &[&str]| -> Vec<String> { list.iter().map(|&arg| arg.into()).collect() };
        let ten = args(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        assert_eq!(substitute_args("$10|$1|$0|$11", &ten), "j|a||");
        assert_eq!(
            substitute_args(
                "${0:-zero} ${1:-} ${2:-two} ${@:-none} ${ARGUMENTS:-none}",
                &args(&["x"])
            ),
            "zero x two x x"
        );
        assert_eq!(
            substitute_args("${@:-none}|${ARGUMENTS:-none}", &[]),
            "none|none"
        );
        assert_eq!(
            substitute_args(
                "${@:0}|${@:2:1}|${@:5}|${@:2:0}|${@:1:99}",
                &args(&["a", "b", "c"])
            ),
            "a b c|b|||a b c"
        );
        assert_eq!(
            substitute_args(
                "$ARGUMENTSX $@@ $$1 ${x} ${@:2:} ${1:-a}b}",
                &args(&["one", "two"])
            ),
            "one twoX one two@ $one ${x} ${@:2:} oneb}"
        );
        assert_eq!(
            substitute_args(
                "$99999999999999999999|${@:99999999999999999999}|${99999999999999999999:-d}",
                &args(&["a"])
            ),
            "||d"
        );
        assert_eq!(
            substitute_args("${1:-multi\nline}|${2:-{nested}", &[]),
            "multi\nline|{nested"
        );
        assert_eq!(
            substitute_args("cost $ and $x and ${", &args(&["a"])),
            "cost $ and $x and ${"
        );
    }
}
