//! The system prompt: named sections the transcript carries and later patches.
//!
//! Port of `packages/coding-agent/src/core/system-prompt.ts` in pi `v1.0.0`. The
//! preamble names yapi, and the pi documentation section is omitted; see
//! `docs/compat.md`.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::resources::{ContextFile, Skill, format_skills};

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
    /// Context files.
    pub context_files: Vec<ContextFile>,
    /// Skills.
    pub skills: Vec<Skill>,
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

/// Resolves the working directory as pi prints it.
pub fn display_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_pi_sections() {
        let mut options = PromptOptions {
            selected_tools: vec!["read".into(), "bash".into()],
            cwd: PathBuf::from("/work"),
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
            ["preamble", "tools", "rules", "cwd"]
        );
        assert_eq!(
            sections["rules"],
            "<rules>\n- Use bash for file operations like ls, rg, find\n- Use read to examine files instead of cat or sed.\n- Be concise in your responses\n- Show file paths clearly when working with files\n</rules>"
        );
        assert_eq!(sections["cwd"], "<cwd>\n/work\n</cwd>");
    }
}
