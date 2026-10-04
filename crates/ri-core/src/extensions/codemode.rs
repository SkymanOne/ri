//! What other parts of a session know about the `codemode` tool, which
//! `ri-ext` implements: its name, schema and prompt text, and how to tell the
//! genuine tool from another extension's tool of the same name.

use serde_json::{Value, json};

use super::ToolInfo;
use crate::tools::Exposure;

/// The tool's name.
pub const NAME: &str = "codemode";

/// Its line in the system prompt's tool list.
pub const SNIPPET: &str = "Run JavaScript that calls other tools";

/// Its rule in the system prompt.
pub const GUIDELINE: &str = "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or filter large output, instead of many separate calls.";

/// Its argument schema.
pub fn parameters() -> Value {
    json!({"type":"object","required":["code"],"properties":{"code":{"type":"string","description":"Raw JavaScript source."}}})
}

/// Whether `tool` is the codemode tool, as pi's `isCodemodeTool`, rather than
/// another extension's tool of that name.
pub fn is_codemode_tool(tool: &ToolInfo) -> bool {
    tool.name == NAME && tool.exposure == Exposure::ModelOnly && tool.parameters == parameters()
}
