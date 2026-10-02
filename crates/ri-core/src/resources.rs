//! Context files, skills, prompt templates and system prompt files.
//!
//! Ports of the discovery parts of `resource-loader.ts`, `skills.ts` and
//! `prompt-templates.ts` in pi `v1.0.0`. Project `.ri/` resources load only for a
//! trusted project; `AGENTS.md` and `CLAUDE.md` files always load.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
pub use ri_types::rpc::SourceInfo;

use crate::config::PROJECT_DIR;
use crate::tools::path::home_dir;

/// A context file and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFile {
    /// Where it was found.
    pub path: PathBuf,
    /// Its text, without a byte order mark.
    pub content: String,
}

/// A skill: instructions the model reads on demand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// Name from frontmatter, else the directory name.
    pub name: String,
    /// When to use it.
    pub description: String,
    /// The skill file.
    pub file_path: PathBuf,
    /// Its directory, for relative references.
    pub base_dir: PathBuf,
    /// Hidden from the prompt; only usable as `/skill:name`.
    pub disable_model_invocation: bool,
    /// Where it was found.
    pub source: SourceInfo,
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

/// Frontmatter values ri reads: strings and booleans.
pub type Frontmatter = IndexMap<String, FrontmatterValue>;

/// A frontmatter scalar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontmatterValue {
    /// A string.
    String(String),
    /// A boolean.
    Bool(bool),
}

impl FrontmatterValue {
    /// The string value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FrontmatterValue::String(text) => Some(text),
            FrontmatterValue::Bool(_) => None,
        }
    }
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        let inner = &trimmed[1..trimmed.len() - 1];
        return if trimmed.starts_with('"') {
            inner.replace("\\\"", "\"").replace("\\n", "\n")
        } else {
            inner.replace("''", "'")
        };
    }
    trimmed.to_owned()
}

/// Splits YAML frontmatter from a markdown body. Supports the subset skills and
/// templates use: `key: value` scalars, quoted strings, booleans and `|` or `>`
/// block scalars.
pub fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
    let normalized = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut frontmatter = Frontmatter::new();
    if !normalized.starts_with("---") {
        return (frontmatter, normalized);
    }
    let Some(end) = normalized[3..].find("\n---").map(|index| index + 3) else {
        return (frontmatter, normalized);
    };
    let yaml = normalized.get(4..end).unwrap_or_default();
    let body = normalized[end + 4..].trim().to_owned();
    let lines: Vec<&str> = yaml.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.starts_with(' ') || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_owned();
        let value = value.trim();
        if value.starts_with('|') || value.starts_with('>') {
            let folded = value.starts_with('>');
            let mut block = Vec::new();
            while index < lines.len() && (lines[index].starts_with(' ') || lines[index].is_empty())
            {
                block.push(lines[index].trim());
                index += 1;
            }
            let text = if folded {
                block.join(" ").trim().to_owned()
            } else {
                block.join("\n").trim_end().to_owned() + "\n"
            };
            frontmatter.insert(key, FrontmatterValue::String(text));
            continue;
        }
        let parsed = match value {
            "true" => FrontmatterValue::Bool(true),
            "false" => FrontmatterValue::Bool(false),
            _ => FrontmatterValue::String(unquote(value)),
        };
        frontmatter.insert(key, parsed);
    }
    (frontmatter, body)
}

/// The resources a package declares in its `package.json`; pi's `PiManifest`.
/// ri reads the `ri` key when present, else pi's `pi` key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Extension entry points, relative to the package.
    pub extensions: Vec<String>,
    /// Skill paths.
    pub skills: Vec<String>,
    /// Prompt template paths.
    pub prompts: Vec<String>,
    /// Theme paths.
    pub themes: Vec<String>,
}

impl Manifest {
    /// The manifest in `package_json`; `None` when the file is unreadable or
    /// declares neither key. A field that is not a list of strings is empty.
    pub fn read(package_json: &Path) -> Option<Manifest> {
        let text = read_text(package_json)?;
        let package: serde_json::Value = serde_json::from_str(&text).ok()?;
        let manifest = ["ri", "pi"]
            .iter()
            .find_map(|key| package.get(*key).filter(|value| value.is_object()))?;
        let field = |name: &str| -> Vec<String> {
            manifest[name]
                .as_array()
                .filter(|entries| entries.iter().all(serde_json::Value::is_string))
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Manifest {
            extensions: field("extensions"),
            skills: field("skills"),
            prompts: field("prompts"),
            themes: field("themes"),
        })
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

/// Discovery metadata for resources under one directory; the path is filled
/// in per resource.
#[derive(Clone)]
struct Origin {
    source: &'static str,
    scope: &'static str,
    base_dir: Option<PathBuf>,
}

impl Origin {
    fn auto(scope: &'static str, base_dir: PathBuf) -> Origin {
        Origin {
            source: "auto",
            scope,
            base_dir: Some(base_dir),
        }
    }

    fn cli() -> Origin {
        Origin {
            source: "cli",
            scope: "temporary",
            base_dir: None,
        }
    }

    fn info(&self, path: &Path) -> SourceInfo {
        SourceInfo {
            path: path.display().to_string(),
            source: self.source.to_owned(),
            scope: self.scope.to_owned(),
            origin: "top-level".to_owned(),
            base_dir: self.base_dir.as_ref().map(|dir| dir.display().to_string()),
        }
    }
}

fn load_skill(path: &Path, origin: &Origin) -> Option<Skill> {
    let (frontmatter, _) = parse_frontmatter(&read_text(path)?);
    let description = frontmatter
        .get("description")
        .and_then(FrontmatterValue::as_str)
        .filter(|text| !text.trim().is_empty())?
        .to_owned();
    let base_dir = path.parent()?.to_path_buf();
    let name = frontmatter
        .get("name")
        .and_then(FrontmatterValue::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            base_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })?;
    Some(Skill {
        name,
        description,
        file_path: path.to_path_buf(),
        base_dir,
        disable_model_invocation: frontmatter.get("disable-model-invocation")
            == Some(&FrontmatterValue::Bool(true)),
        source: origin.info(path),
    })
}

/// Skills under `dir`: a directory with `SKILL.md` is one skill; otherwise
/// subdirectories are searched, and at the top level other `.md` files with a
/// description are skills too. Hidden entries and `node_modules` are skipped.
fn skills_in(dir: &Path, top: bool, origin: &Origin, skills: &mut Vec<Skill>) {
    let skill_file = dir.join("SKILL.md");
    if skill_file.is_file() {
        skills.extend(load_skill(&skill_file, origin));
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
            skills_in(&path, false, origin, skills);
        } else if top && path.is_file() && name.ends_with(".md") {
            skills.extend(load_skill(&path, origin));
        }
    }
}

/// Skills from the user's and the project's skill directories and `extra` paths.
/// The first skill of a name wins.
pub fn skills(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    extra: &[PathBuf],
) -> Vec<Skill> {
    let mut found = Vec::new();
    let user_agents = home_dir().join(".agents");
    skills_in(
        &agent_dir.join("skills"),
        true,
        &Origin::auto("user", agent_dir.to_path_buf()),
        &mut found,
    );
    skills_in(
        &user_agents.join("skills"),
        true,
        &Origin::auto("user", user_agents.clone()),
        &mut found,
    );
    if project_trusted {
        let project = cwd.join(PROJECT_DIR);
        skills_in(
            &project.join("skills"),
            true,
            &Origin::auto("project", project.clone()),
            &mut found,
        );
        for dir in cwd.ancestors() {
            let agents = dir.join(".agents");
            if agents != user_agents {
                skills_in(
                    &agents.join("skills"),
                    true,
                    &Origin::auto("project", agents.clone()),
                    &mut found,
                );
            }
        }
    }
    for path in extra {
        if path.is_dir() {
            skills_in(path, true, &Origin::cli(), &mut found);
        } else if path.is_file() {
            found.extend(load_skill(path, &Origin::cli()));
        }
    }
    let mut unique: Vec<Skill> = Vec::new();
    for skill in found {
        let canonical = std::fs::canonicalize(&skill.file_path).ok();
        let duplicate = unique.iter().any(|existing| {
            existing.name == skill.name
                || std::fs::canonicalize(&existing.file_path).ok() == canonical
        });
        if !duplicate {
            unique.push(skill);
        }
    }
    unique
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

fn templates_in(dir: &Path, origin: &Origin, templates: &mut Vec<PromptTemplate>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = read.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if !path.is_file() || path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        let (frontmatter, body) = parse_frontmatter(&text);
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut description = frontmatter
            .get("description")
            .and_then(FrontmatterValue::as_str)
            .unwrap_or_default()
            .to_owned();
        if description.is_empty()
            && let Some(first) = body.lines().find(|line| !line.trim().is_empty())
        {
            let units: Vec<u16> = first.encode_utf16().collect();
            description = String::from_utf16_lossy(&units[..units.len().min(60)]);
            if units.len() > 60 {
                description += "...";
            }
        }
        templates.push(PromptTemplate {
            name,
            description,
            argument_hint: frontmatter
                .get("argument-hint")
                .and_then(FrontmatterValue::as_str)
                .map(str::to_owned),
            content: body,
            source: origin.info(&path),
            file_path: path,
        });
    }
}

/// Templates from the user's and the trusted project's `prompts` directories and
/// `extra` paths.
pub fn prompt_templates(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    extra: &[PathBuf],
) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();
    templates_in(
        &agent_dir.join("prompts"),
        &Origin::auto("user", agent_dir.to_path_buf()),
        &mut templates,
    );
    if project_trusted {
        let project = cwd.join(PROJECT_DIR);
        templates_in(
            &project.join("prompts"),
            &Origin::auto("project", project.clone()),
            &mut templates,
        );
    }
    for path in extra {
        if path.is_dir() {
            templates_in(path, &Origin::cli(), &mut templates);
        }
    }
    templates
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

/// Replaces `$1`, `$@`, `$ARGUMENTS`, `${N:-default}` and `${@:start:length}`.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut index = 0;
    let digits = |from: usize| {
        chars[from..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count()
    };
    while index < chars.len() {
        if chars[index] != '$' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let rest: String = chars[index + 1..].iter().collect();
        if rest.starts_with('{')
            && let Some(close) = rest.find('}')
        {
            let inner = &rest[1..close];
            let consumed = 1 + close + 1;
            if let Some((target, default)) = inner.split_once(":-") {
                let valid = target == "@"
                    || target == "ARGUMENTS"
                    || (!target.is_empty() && target.chars().all(|c| c.is_ascii_digit()));
                if valid && !default.contains('}') {
                    let value = if target == "@" || target == "ARGUMENTS" {
                        Some(all.clone())
                    } else {
                        target
                            .parse::<usize>()
                            .ok()
                            .and_then(|n| n.checked_sub(1))
                            .and_then(|i| args.get(i).cloned())
                    };
                    out.push_str(
                        &value
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| default.to_owned()),
                    );
                    index += consumed;
                    continue;
                }
            }
            if let Some(slice) = inner.strip_prefix("@:") {
                let number = |text: &str| {
                    (!text.is_empty() && text.chars().all(|c| c.is_ascii_digit()))
                        .then(|| text.parse::<usize>().ok())
                        .flatten()
                };
                let (start, length) = match slice.split_once(':') {
                    Some((start, length)) => (number(start), Some(number(length))),
                    None => (number(slice), None),
                };
                if let Some(start) = start
                    && length.is_none_or(|length| length.is_some())
                {
                    let start = start.saturating_sub(1).min(args.len());
                    let end = match length {
                        Some(Some(length)) => (start + length).min(args.len()),
                        _ => args.len(),
                    };
                    out.push_str(&args[start..end].join(" "));
                    index += consumed;
                    continue;
                }
            }
        }
        if rest.starts_with("ARGUMENTS") {
            out.push_str(&all);
            index += 1 + "ARGUMENTS".len();
            continue;
        }
        if rest.starts_with('@') {
            out.push_str(&all);
            index += 2;
            continue;
        }
        let count = digits(index + 1);
        if count > 0 {
            let number: String = chars[index + 1..index + 1 + count].iter().collect();
            let value = number
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| args.get(i).cloned())
                .unwrap_or_default();
            out.push_str(&value);
            index += 1 + count;
            continue;
        }
        out.push('$');
        index += 1;
    }
    out
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
            "---\nname: demo\ndescription: \"Does: things\"\ndisable-model-invocation: true\nnotes: |\n  a\n  b\n---\n\nBody\n",
        );
        assert_eq!(meta["name"], FrontmatterValue::String("demo".into()));
        assert_eq!(
            meta["description"],
            FrontmatterValue::String("Does: things".into())
        );
        assert_eq!(
            meta["disable-model-invocation"],
            FrontmatterValue::Bool(true)
        );
        assert_eq!(meta["notes"], FrontmatterValue::String("a\nb\n".into()));
        assert_eq!(body, "Body");
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
}
