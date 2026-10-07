//! pi's system prompt inputs, as `before_agent_start` hands them to extensions.
//!
//! Mirrors `NormalizedBuildSystemPromptOptions` in
//! `packages/coding-agent/src/core/system-prompt.ts` and `Skill` in
//! `core/skills.ts` in pi `v1.0.0`.

use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::rpc::SourceInfo;

/// pi's normalized `BuildSystemPromptOptions`. Handlers edit it and hand it
/// back, so a key they removed takes its empty value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SystemPromptOptions {
    /// Replaces the default preamble, tools, rules and docs (`SYSTEM.md`,
    /// `--system-prompt`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_prompt: Option<String>,
    /// The whole prompt, as a `before_agent_start` handler set it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_system_prompt: Option<String>,
    /// Active tool names, in order.
    pub selected_tools: Vec<String>,
    /// One-line summaries by tool name; tools without one are not listed.
    pub tool_snippets: IndexMap<String, String>,
    /// Rule bullets by tool name.
    pub tool_guidelines: IndexMap<String, Vec<String>>,
    /// Extra rule bullets.
    pub prompt_guidelines: Vec<String>,
    /// Appended after the rules (`APPEND_SYSTEM.md`, `--append-system-prompt`);
    /// empty for none.
    pub append_system_prompt: String,
    /// Extra sections from extensions, by tag name.
    pub sections: IndexMap<String, String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Context files.
    pub context_files: Vec<ContextFile>,
    /// Skills.
    pub skills: Vec<Skill>,
}

/// A context file and its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile {
    /// Where it was found.
    pub path: PathBuf,
    /// Its text, without a byte order mark.
    pub content: String,
}

/// A skill: instructions the model reads on demand. A handler may add one
/// with only the fields the prompt shows; the others take their empty value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Skill {
    /// Name from frontmatter, else the directory name.
    pub name: String,
    /// When to use it.
    pub description: String,
    /// The skill file.
    pub file_path: PathBuf,
    /// Its directory, for relative references.
    pub base_dir: PathBuf,
    /// Where it was found.
    #[serde(rename = "sourceInfo")]
    pub source: SourceInfo,
    /// Hidden from the prompt; only usable as `/skill:name`.
    pub disable_model_invocation: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The options keep the key order of pi's `normalizeBuildSystemPromptOptions`
    /// and leave out unset optional keys, as `JSON.stringify` does.
    #[test]
    fn serializes_in_pi_order() {
        let mut options = SystemPromptOptions {
            custom_prompt: Some("custom".into()),
            force_system_prompt: Some("forced".into()),
            selected_tools: vec!["read".into(), "bash".into()],
            prompt_guidelines: vec!["g".into()],
            append_system_prompt: "app".into(),
            cwd: "/work".into(),
            context_files: vec![ContextFile {
                path: "/work/AGENTS.md".into(),
                content: "c".into(),
            }],
            skills: vec![Skill {
                name: "s".into(),
                description: "d".into(),
                file_path: "/s/SKILL.md".into(),
                base_dir: "/s".into(),
                source: SourceInfo {
                    path: "/s/SKILL.md".into(),
                    source: "auto".into(),
                    scope: "user".into(),
                    origin: "top-level".into(),
                    base_dir: Some("/s".into()),
                },
                disable_model_invocation: true,
            }],
            ..SystemPromptOptions::default()
        };
        options.tool_snippets.insert("read".into(), "Read".into());
        options
            .tool_guidelines
            .insert("read".into(), vec!["r".into()]);
        options.sections.insert("notes".into(), "n".into());
        assert_eq!(
            crate::json::to_string(&options).unwrap(),
            r#"{"customPrompt":"custom","forceSystemPrompt":"forced","selectedTools":["read","bash"],"toolSnippets":{"read":"Read"},"toolGuidelines":{"read":["r"]},"promptGuidelines":["g"],"appendSystemPrompt":"app","sections":{"notes":"n"},"cwd":"/work","contextFiles":[{"path":"/work/AGENTS.md","content":"c"}],"skills":[{"name":"s","description":"d","filePath":"/s/SKILL.md","baseDir":"/s","sourceInfo":{"path":"/s/SKILL.md","source":"auto","scope":"user","origin":"top-level","baseDir":"/s"},"disableModelInvocation":true}]}"#
        );
        assert_eq!(
            crate::json::to_string(&SystemPromptOptions::default()).unwrap(),
            r#"{"selectedTools":[],"toolSnippets":{},"toolGuidelines":{},"promptGuidelines":[],"appendSystemPrompt":"","sections":{},"cwd":"","contextFiles":[],"skills":[]}"#
        );
    }

    /// A handler's edit may drop keys and add a skill with only some fields.
    #[test]
    fn reads_partial_edits() {
        let options: SystemPromptOptions = serde_json::from_str(
            r#"{"selectedTools":["read"],"skills":[{"name":"s","description":"d","filePath":"/s/SKILL.md"}]}"#,
        )
        .unwrap();
        assert_eq!(options.selected_tools, ["read"]);
        assert_eq!(options.cwd, PathBuf::new());
        assert_eq!(options.skills[0].name, "s");
        assert!(!options.skills[0].disable_model_invocation);
    }
}
