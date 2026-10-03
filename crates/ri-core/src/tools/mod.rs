//! Built-in tools: `read`, `bash`, `edit`, `write`, and the optional `grep`,
//! `find` and `ls`.
//!
//! Ports of `packages/coding-agent/src/core/tools` in pi `v1.0.0`. Declarations
//! (names, descriptions, schemas) match pi byte for byte, because they are part of
//! every request.

pub mod bash;
mod edit;
mod edit_diff;
pub mod external;
mod find;
mod grep;
mod ls;
mod mutation;
pub mod path;
mod read;
pub mod registry;
pub mod truncate;
mod write;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;
use ri_agent::{Tool, UpdateSink};
use ri_types::event::ToolResult;
use ri_types::message::{ContentBlock, TextContent, ThinkingLevel, ToolDeclaration};
use ri_types::model::Model;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use bash::Bash;
pub use edit::Edit;
pub use find::Find;
pub use grep::Grep;
pub use ls::Ls;
pub use read::{Read, base64, image_mime_type};
pub use write::Write;

/// Names of the tools active by default.
pub const DEFAULT_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];

/// Every built-in tool, in pi's registration order.
pub const BUILTIN_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

/// Session state tools read while running.
#[derive(Clone, Debug, Default)]
pub struct Runtime {
    /// The current model.
    pub model: Option<Model>,
    /// The current thinking level.
    pub thinking_level: Option<ThinkingLevel>,
    /// The session id.
    pub session_id: Option<String>,
    /// The session file, when persisted.
    pub session_file: Option<PathBuf>,
}

/// What built-in tools share: the working directory, the session state and
/// where helper binaries live.
#[derive(Clone, Debug)]
pub struct ToolEnv {
    /// Working directory for relative paths and commands.
    pub cwd: PathBuf,
    /// Session state, updated by the session.
    pub runtime: Arc<RwLock<Runtime>>,
    /// The agent's `bin` directory: prepended to `PATH` for commands, and where
    /// `rg` and `fd` are installed when missing.
    pub bin_dir: PathBuf,
}

impl ToolEnv {
    /// A snapshot of the session state.
    pub fn runtime(&self) -> Runtime {
        self.runtime.read().map(|r| r.clone()).unwrap_or_default()
    }
}

/// How a tool reaches the model; pi's `ToolExposure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Exposure {
    /// Declared to the model while active, and callable from other tools.
    Direct,
    /// Declared while active, but not callable from other tools.
    ModelOnly,
    /// Callable from codemode scripts; never declared.
    Codemode,
    /// Declared once `tool_search` loads it.
    Deferred,
    /// Registered but unreachable.
    Hidden,
}

impl Exposure {
    /// Whether activating the tool declares it to the model.
    pub fn declarable(self) -> bool {
        matches!(self, Exposure::Direct | Exposure::ModelOnly)
    }
}

/// A group of tools, such as one MCP server's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Namespace {
    /// Name, such as `mcp__docs`.
    pub name: String,
    /// What the group offers.
    pub description: Option<String>,
    /// How to use the group's tools.
    pub instructions: Option<String>,
}

/// A tool declared with another description; everything else is the tool's.
pub struct Described {
    tool: Arc<dyn Tool>,
    declaration: ToolDeclaration,
}

impl Described {
    /// `tool`, declared with `description`.
    pub fn new(tool: Arc<dyn Tool>, description: String) -> Described {
        let declaration = ToolDeclaration {
            description,
            ..tool.declaration().clone()
        };
        Described { tool, declaration }
    }
}

impl Tool for Described {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn label(&self) -> &str {
        self.tool.label()
    }

    fn execution_mode(&self) -> ri_agent::ExecutionMode {
        self.tool.execution_mode()
    }

    fn output_schema(&self) -> Option<&Value> {
        self.tool.output_schema()
    }

    fn prepare_arguments(
        &self,
        arguments: serde_json::Map<String, Value>,
    ) -> serde_json::Map<String, Value> {
        self.tool.prepare_arguments(arguments)
    }

    fn execute(
        &self,
        call_id: String,
        args: Value,
        cancel: CancellationToken,
        updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        self.tool.execute(call_id, args, cancel, updates)
    }
}

/// A tool as registered with a session: the tool, the text it adds to the
/// system prompt, and how it reaches the model.
#[derive(Clone)]
pub struct RegisteredTool {
    /// The tool.
    pub tool: Arc<dyn Tool>,
    /// One-line summary for the prompt's tool list; absent tools are not listed.
    pub snippet: Option<String>,
    /// Bullets added to the prompt's rules.
    pub guidelines: Vec<String>,
    /// How it reaches the model.
    pub exposure: Exposure,
    /// Its group.
    pub namespace: Option<Namespace>,
    /// Whether registering it activates it, when its exposure is declarable.
    pub default_active: bool,
}

impl RegisteredTool {
    /// A direct tool, active on registration.
    pub fn direct(tool: Arc<dyn Tool>, snippet: Option<String>, guidelines: Vec<String>) -> Self {
        RegisteredTool {
            tool,
            snippet,
            guidelines,
            exposure: Exposure::Direct,
            namespace: None,
            default_active: true,
        }
    }

    /// The tool's name.
    pub fn name(&self) -> &str {
        &self.tool.declaration().name
    }
}

/// Creates the named built-in tool.
pub fn builtin(name: &str, env: &ToolEnv) -> Option<RegisteredTool> {
    let (tool, snippet, guidelines): (Arc<dyn Tool>, &str, &[&str]) = match name {
        "read" => (
            Arc::new(Read::new(env.clone())),
            "Read file contents",
            &["Use read to examine files instead of cat or sed."],
        ),
        "bash" => (
            Arc::new(Bash::new(env.clone())),
            "Execute bash commands (ls, grep, find, etc.)",
            &["You can inspect PI_* environment variables for current model and session details."],
        ),
        "edit" => (
            Arc::new(Edit::new(env.clone())),
            "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
            &[
                "Use edit for precise changes (edits[].oldText must match exactly)",
                "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
                "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
                "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
            ],
        ),
        "write" => (
            Arc::new(Write::new(env.clone())),
            "Create or overwrite files",
            &["Use write only for new files or complete rewrites."],
        ),
        "grep" => (
            Arc::new(Grep::new(env.clone())),
            "Search file contents for patterns (respects .gitignore)",
            &[],
        ),
        "find" => (
            Arc::new(Find::new(env.clone())),
            "Find files by glob pattern (respects .gitignore)",
            &[],
        ),
        "ls" => (
            Arc::new(Ls::new(env.clone())),
            "List directory contents",
            &[],
        ),
        _ => return None,
    };
    Some(RegisteredTool::direct(
        tool,
        Some(snippet.to_owned()),
        guidelines.iter().map(|g| (*g).to_owned()).collect(),
    ))
}

/// A declaration with pi's `constrainedSampling: {type: json_schema, strict: prefer}`,
/// as the core tools declare.
fn declaration(name: &str, description: String, parameters: Value) -> ToolDeclaration {
    ToolDeclaration {
        constrained_sampling: Some(serde_json::json!({"type": "json_schema", "strict": "prefer"})),
        ..plain_declaration(name, description, parameters)
    }
}

/// A declaration without constrained sampling, as the search tools declare.
fn plain_declaration(name: &str, description: String, parameters: Value) -> ToolDeclaration {
    ToolDeclaration {
        name: name.to_owned(),
        description,
        parameters,
        constrained_sampling: None,
    }
}

fn text(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn text_result(content: impl Into<String>, details: Option<Value>) -> ToolResult {
    ToolResult {
        content: vec![text(content)],
        details,
        ..ToolResult::default()
    }
}

/// `Error code: ENOENT` style text for an I/O error, as Node reports it.
fn error_code(err: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let code = match err.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::IsADirectory => "EISDIR",
        ErrorKind::NotADirectory => "ENOTDIR",
        _ => return err.to_string(),
    };
    format!("Error code: {code}")
}

/// The message Node gives a failed file system call, such as
/// `ENOENT: no such file or directory, access '/a/b'`.
fn node_error(err: &std::io::Error, syscall: &str, path: &std::path::Path) -> String {
    use std::io::ErrorKind;
    let path = path.display();
    match err.kind() {
        ErrorKind::NotFound => format!("ENOENT: no such file or directory, {syscall} '{path}'"),
        ErrorKind::PermissionDenied => format!("EACCES: permission denied, {syscall} '{path}'"),
        ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".to_owned(),
        ErrorKind::NotADirectory => format!("ENOTDIR: not a directory, {syscall} '{path}'"),
        _ => format!("{err}, {syscall} '{path}'"),
    }
}

/// A number as JavaScript prints it.
pub fn js_number(value: f64) -> String {
    ri_types::json::to_string(&value).unwrap_or_default()
}

/// Output capped at the default byte limit with no line limit, then pi's
/// bracketed notices: `before`, the byte limit if hit, `after`. The truncation
/// goes into `details` when it applied.
fn capped_output(
    raw: &str,
    before: Vec<String>,
    after: Vec<String>,
    details: &mut serde_json::Map<String, Value>,
) -> String {
    let mut notices = before;
    let truncation = truncate::truncate_head(raw, JS_MAX_SAFE_INTEGER, truncate::DEFAULT_MAX_BYTES);
    let mut output = truncation.content.clone();
    if truncation.truncated {
        notices.push(format!(
            "{} limit reached",
            truncate::format_size(truncate::DEFAULT_MAX_BYTES)
        ));
        details.insert(
            "truncation".into(),
            serde_json::to_value(&truncation).unwrap_or_default(),
        );
    }
    notices.extend(after);
    if !notices.is_empty() {
        output += &format!("\n\n[{}]", notices.join(". "));
    }
    output
}

/// `Number.MAX_SAFE_INTEGER`, pi's "no line limit".
const JS_MAX_SAFE_INTEGER: usize = 9_007_199_254_740_991;

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let _ = getrandom::fill(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}
