//! The system prompt: named sections the transcript carries and later patches.
//!
//! Port of `packages/coding-agent/src/core/system-prompt.ts` in pi `v1.0.0`. The
//! preamble and the documentation section name yapi, and the section points
//! to the local docs, or to the published ones without a local copy.

use std::path::PathBuf;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::resources::{ContextFile, Skill, SourceInfo, format_skills};

/// Inputs for the prompt.
#[derive(Clone, Debug, Default)]
pub struct PromptOptions {
    /// Replaces the default preamble, tools, rules and docs (`SYSTEM.md`,
    /// `--system-prompt`).
    pub custom_prompt: Option<String>,
    /// Active tool names, in order.
    pub selected_tools: Vec<String>,
    /// One-line summaries by tool name; tools without one are not listed.
    pub tool_snippets: IndexMap<String, String>,
    /// Rule bullets by tool name.
    pub tool_guidelines: IndexMap<String, Vec<String>>,
    /// Extra rule bullets.
    pub prompt_guidelines: Vec<String>,
    /// Appended after the rules (`APPEND_SYSTEM.md`, `--append-system-prompt`).
    pub append: Option<String>,
    /// Extra sections from extensions, by tag name.
    pub sections: IndexMap<String, String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Where the model reads yapi's and pi's docs.
    pub docs: crate::docs::Locations,
    /// Context files.
    pub context_files: Vec<ContextFile>,
    /// Skills.
    pub skills: Vec<Skill>,
}

impl PromptOptions {
    /// pi's normalized `BuildSystemPromptOptions`, as `before_agent_start`
    /// hands them to extensions, with `forced` as `forceSystemPrompt`.
    pub fn to_json(&self, forced: Option<&str>) -> Value {
        let mut out = Map::new();
        if let Some(prompt) = &self.custom_prompt {
            out.insert("customPrompt".into(), json!(prompt));
        }
        if let Some(forced) = forced {
            out.insert("forceSystemPrompt".into(), json!(forced));
        }
        out.insert("selectedTools".into(), json!(self.selected_tools));
        out.insert("toolSnippets".into(), json!(self.tool_snippets));
        out.insert("toolGuidelines".into(), json!(self.tool_guidelines));
        out.insert("promptGuidelines".into(), json!(self.prompt_guidelines));
        out.insert(
            "appendSystemPrompt".into(),
            json!(self.append.as_deref().unwrap_or_default()),
        );
        out.insert("sections".into(), json!(self.sections));
        out.insert("cwd".into(), json!(self.cwd));
        out.insert(
            "contextFiles".into(),
            self.context_files
                .iter()
                .map(|file| json!({"path": file.path, "content": file.content}))
                .collect(),
        );
        out.insert(
            "skills".into(),
            self.skills
                .iter()
                .map(|skill| {
                    json!({
                        "name": skill.name,
                        "description": skill.description,
                        "filePath": skill.file_path,
                        "baseDir": skill.base_dir,
                        "sourceInfo": skill.source,
                        "disableModelInvocation": skill.disable_model_invocation,
                    })
                })
                .collect(),
        );
        Value::Object(out)
    }

    /// Takes the options an extension edited in `value`, pi's shape from
    /// [`PromptOptions::to_json`]. A field it sent malformed keeps its value.
    /// Returns the `forceSystemPrompt` it left.
    pub fn apply_json(&mut self, value: &Value) -> Option<String> {
        fn field<T: serde::de::DeserializeOwned>(value: &Value, key: &str) -> Option<T> {
            serde_json::from_value(value.get(key)?.clone()).ok()
        }
        self.custom_prompt = field(value, "customPrompt");
        self.append = field::<String>(value, "appendSystemPrompt").filter(|text| !text.is_empty());
        if let Some(tools) = field(value, "selectedTools") {
            self.selected_tools = tools;
        }
        if let Some(snippets) = field(value, "toolSnippets") {
            self.tool_snippets = snippets;
        }
        if let Some(guidelines) = field(value, "toolGuidelines") {
            self.tool_guidelines = guidelines;
        }
        if let Some(guidelines) = field(value, "promptGuidelines") {
            self.prompt_guidelines = guidelines;
        }
        if let Some(sections) = field(value, "sections") {
            self.sections = sections;
        }
        if let Some(cwd) = field(value, "cwd") {
            self.cwd = cwd;
        }
        if let Some(files) = field::<Vec<Value>>(value, "contextFiles") {
            self.context_files = files
                .iter()
                .filter_map(|file| {
                    Some(ContextFile {
                        path: field(file, "path")?,
                        content: field(file, "content")?,
                    })
                })
                .collect();
        }
        if let Some(skills) = field::<Vec<Value>>(value, "skills") {
            self.skills = skills
                .iter()
                .filter_map(|skill| {
                    Some(Skill {
                        name: field(skill, "name")?,
                        description: field(skill, "description")?,
                        file_path: field(skill, "filePath")?,
                        base_dir: field(skill, "baseDir")?,
                        disable_model_invocation: field(skill, "disableModelInvocation")
                            .unwrap_or(false),
                        source: field::<SourceInfo>(skill, "sourceInfo")?,
                    })
                })
                .collect();
        }
        field(value, "forceSystemPrompt")
    }
}

fn rules(options: &PromptOptions) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut add = |rule: &str| {
        let rule = rule.trim();
        if !rule.is_empty() && !rules.iter().any(|existing| existing == rule) {
            rules.push(rule.to_owned());
        }
    };
    let has = |name: &str| options.selected_tools.iter().any(|tool| tool == name);
    if (has("bash") || has("powershell")) && !has("grep") && !has("find") && !has("ls") {
        if has("bash") && has("powershell") {
            add(
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
            );
        } else if has("powershell") {
            add("Use PowerShell for file operations like listing, searching, and finding files");
        } else {
            add("Use bash for file operations like ls, rg, find");
        }
    }
    for name in &options.selected_tools {
        for rule in options.tool_guidelines.get(name).into_iter().flatten() {
            add(rule);
        }
    }
    for rule in &options.prompt_guidelines {
        add(rule);
    }
    add("Be concise in your responses");
    add("Show file paths clearly when working with files");
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// pi's `docs` section, naming yapi, with where the docs are: yapi's main
/// page, and pi's docs and examples.
fn docs(docs: &crate::docs::Locations) -> String {
    format!(
        "yapi documentation (read only when the user asks about yapi itself, its SDK, extensions, themes, skills, or TUI):
- Main documentation: {}
- Additional docs: {}
- Examples: {} (extensions, custom tools, SDK)
- When reading yapi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory
- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md), environment variables (docs/environment-variables.md), MCP servers (docs/mcp.md), codemode scripts and non-LLM models such as classifiers and image models (docs/codemode.md)
- When working on yapi topics, read the docs and examples, and follow .md cross-references before implementing
- Always read yapi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
        docs.main, docs.pi_docs, docs.pi_examples,
    )
}

/// Whether a custom section name is valid: lowercase, then letters, digits, `_`,
/// `-`; not `preamble`.
pub fn valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    name != "preamble"
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The prompt's sections in order. `preamble` is plain text; every other section
/// is wrapped in a tag of its name.
pub fn build_sections(options: &PromptOptions) -> Result<IndexMap<String, String>, String> {
    for name in options.sections.keys() {
        if !valid_section_name(name) {
            return Err(format!("Invalid system prompt section name: {name}"));
        }
    }
    let mut raw: IndexMap<String, String> = IndexMap::new();
    match &options.custom_prompt {
        Some(custom) if !custom.is_empty() => {
            raw.insert("preamble".into(), custom.clone());
        }
        _ => {
            raw.insert(
                "preamble".into(),
                "You are an expert coding assistant operating inside yapi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.".into(),
            );
            let visible: Vec<String> = options
                .selected_tools
                .iter()
                .filter_map(|name| {
                    options
                        .tool_snippets
                        .get(name)
                        .filter(|snippet| !snippet.is_empty())
                        .map(|snippet| format!("- {name}: {snippet}"))
                })
                .collect();
            let tools = if visible.is_empty() {
                "(none)".to_owned()
            } else {
                visible.join("\n")
            };
            raw.insert(
                "tools".into(),
                format!("{tools}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."),
            );
            raw.insert("rules".into(), rules(options));
            raw.insert("docs".into(), docs(&options.docs));
        }
    }
    if let Some(append) = options.append.as_ref().filter(|text| !text.is_empty()) {
        raw.insert("addendum".into(), append.clone());
    }
    if !options.context_files.is_empty() {
        let mut parts = vec!["Project-specific instructions and guidelines:".to_owned()];
        for file in &options.context_files {
            parts.push(format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                file.path.display(),
                file.content
            ));
        }
        raw.insert("project_context".into(), parts.join("\n\n"));
    }
    let read_tool = ["read", "bash"].into_iter().find(|tool| {
        options
            .selected_tools
            .iter()
            .any(|selected| selected == tool)
    });
    if let Some(read_tool) = read_tool
        && !options.skills.is_empty()
    {
        let skills = format_skills(&options.skills, read_tool).trim().to_owned();
        if !skills.is_empty() {
            raw.insert("skills".into(), skills);
        }
    }
    raw.insert(
        "cwd".into(),
        options.cwd.to_string_lossy().replace('\\', "/"),
    );
    for (name, content) in &options.sections {
        if !content.is_empty() {
            raw.insert(name.clone(), content.clone());
        }
    }
    let mut sections = IndexMap::new();
    for (name, content) in raw {
        if name == "preamble" {
            sections.insert(name, content);
        } else {
            let wrapped = format!("<{name}>\n{content}\n</{name}>");
            sections.insert(name, wrapped);
        }
    }
    Ok(sections)
}

/// The section patch from what the model has to what it should have: changed or
/// new sections, and `None` for removed ones. `None` when nothing changed.
pub fn diff_sections(
    previous: &IndexMap<String, Option<String>>,
    current: &IndexMap<String, String>,
) -> Option<IndexMap<String, Option<String>>> {
    let mut patch = IndexMap::new();
    for (name, text) in current {
        if previous.get(name).and_then(Option::as_ref) != Some(text) {
            patch.insert(name.clone(), Some(text.clone()));
        }
    }
    for name in previous.keys() {
        if !current.contains_key(name) {
            patch.insert(name.clone(), None);
        }
    }
    (!patch.is_empty()).then_some(patch)
}

/// The prompt as plain text, as the transcript renders it.
pub fn render(sections: &IndexMap<String, String>) -> String {
    sections
        .values()
        .filter(|text| !text.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_pi_sections() {
        let mut options = PromptOptions {
            selected_tools: vec!["read".into(), "bash".into()],
            cwd: PathBuf::from("/work"),
            docs: crate::docs::Locations {
                main: "/agent/docs/index.md".into(),
                pi_docs: "/agent/docs/pi/docs".into(),
                pi_examples: "/agent/docs/pi/examples".into(),
            },
            ..PromptOptions::default()
        };
        options
            .tool_snippets
            .insert("read".into(), "Read file contents".into());
        options.tool_snippets.insert(
            "bash".into(),
            "Execute bash commands (ls, grep, find, etc.)".into(),
        );
        options.tool_guidelines.insert(
            "read".into(),
            vec!["Use read to examine files instead of cat or sed.".into()],
        );
        let sections = build_sections(&options).unwrap();
        assert_eq!(
            sections.keys().cloned().collect::<Vec<_>>(),
            ["preamble", "tools", "rules", "docs", "cwd"]
        );
        assert_eq!(
            sections["rules"],
            "<rules>\n- Use bash for file operations like ls, rg, find\n- Use read to examine files instead of cat or sed.\n- Be concise in your responses\n- Show file paths clearly when working with files\n</rules>"
        );
        assert!(sections["docs"].starts_with(
            "<docs>\nyapi documentation (read only when the user asks about yapi itself, its SDK, extensions, themes, skills, or TUI):\n- Main documentation: /agent/docs/index.md\n- Additional docs: /agent/docs/pi/docs\n- Examples: /agent/docs/pi/examples (extensions, custom tools, SDK)\n"
        ));
        assert_eq!(sections["cwd"], "<cwd>\n/work\n</cwd>");
    }
}
