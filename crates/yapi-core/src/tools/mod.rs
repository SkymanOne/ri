//! Built-in tools: `read`, `bash`, `edit`, `write`, and the optional `grep`,
//! `find` and `ls`.
//!
//! Ports of `packages/coding-agent/src/core/tools` in pi `v1.0.0`. Declarations
//! (names, descriptions, schemas) match pi byte for byte, because they are part of
//! every request.

pub mod bash;
mod edit;
mod edit_diff;

pub use edit_diff::{Replacement, preview_edits};
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
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_types::event::ToolResult;
use yapi_types::message::{ContentBlock, ThinkingLevel, ToolDeclaration};
use yapi_types::model::Model;

pub use bash::Bash;
pub use edit::Edit;
pub use find::Find;
pub use grep::Grep;
pub use ls::Ls;
pub use read::{Read, image_mime_type};
pub use write::Write;

/// Names of the tools active by default.
pub const DEFAULT_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];

/// Every built-in tool, in pi's registration order.
pub const BUILTIN_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

/// Session state tools read while running.
#[derive(Clone, Debug)]
pub struct Runtime {
    /// The current model.
    pub model: Option<Model>,
    /// The current thinking level.
    pub thinking_level: Option<ThinkingLevel>,
    /// The session id.
    pub session_id: Option<String>,
    /// The session file, when persisted.
    pub session_file: Option<PathBuf>,
    /// The `images.autoResize` setting.
    pub auto_resize_images: bool,
}

impl Default for Runtime {
    /// No session state. Images resize, as in pi's tool factories.
    fn default() -> Runtime {
        Runtime {
            model: None,
            thinking_level: None,
            session_id: None,
            session_file: None,
            auto_resize_images: true,
        }
    }
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
        yapi_types::sync::read(&self.runtime).clone()
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

    fn execution_mode(&self) -> yapi_agent::ExecutionMode {
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

/// pi's `Operation aborted` once `cancel` fired.
fn check_abort(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("Operation aborted".to_owned())
    } else {
        Ok(())
    }
}

/// Sleeps until `deadline`; without one, never wakes.
pub(crate) async fn sleep_until_opt(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

fn text_result(content: impl Into<String>, details: Option<Value>) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::text(content)],
        details,
        ..ToolResult::default()
    }
}

pub use yapi_types::js::node_error;

/// `Error code: ENOENT` style text for an I/O error, as Node reports it.
pub(crate) fn error_code(err: &std::io::Error) -> String {
    match yapi_types::js::errno(err.kind()) {
        Some((code, _)) => format!("Error code: {code}"),
        None => err.to_string(),
    }
}

/// A number as JavaScript prints it.
pub fn js_number(value: f64) -> String {
    yapi_types::json::to_string(&value).unwrap_or_default()
}

/// The result of output capped at the default byte limit with no line limit,
/// then pi's bracketed notices: `before`, the byte limit if hit, `after`. The
/// truncation joins `details` when it applied; empty details are left out.
fn capped_output(
    raw: &str,
    before: Vec<String>,
    after: Vec<String>,
    mut details: serde_json::Map<String, Value>,
) -> ToolResult {
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
    text_result(
        output,
        (!details.is_empty()).then_some(Value::Object(details)),
    )
}

/// `Number.MAX_SAFE_INTEGER`, pi's "no line limit".
const JS_MAX_SAFE_INTEGER: usize = 9_007_199_254_740_991;
