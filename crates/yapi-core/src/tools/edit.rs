//! `edit`: exact text replacements in one file, preserving BOM and line endings.

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_types::event::ToolResult;
use yapi_types::message::ToolDeclaration;

use super::edit_diff::{
    Replacement, apply_edits, detect_line_ending, display_diff, normalize_to_lf,
    restore_line_endings, unified_patch,
};
use super::mutation::with_file_lock;
use super::{ToolEnv, check_abort, declaration, error_code, text_result};

/// The `edit` tool.
pub struct Edit {
    env: ToolEnv,
    declaration: ToolDeclaration,
}

impl Edit {
    /// An `edit` tool for `env`.
    pub fn new(env: ToolEnv) -> Edit {
        Edit {
            env,
            declaration: declaration(
                "edit",
                "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".into(),
                json!({"type":"object","required":["path","edits"],"properties":{
                    "path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},
                    "edits":{"type":"array","items":{"type":"object","required":["oldText","newText"],"properties":{
                        "oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},
                        "newText":{"type":"string","description":"Replacement text for this targeted edit."}}},
                        "description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."}}}),
            ),
        }
    }
}

fn is_single_edit(value: &Value) -> bool {
    value["oldText"].is_string() && value["newText"].is_string()
}

impl Tool for Edit {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    /// Accepts `edits` as a JSON string or a single object, and the legacy
    /// top-level `oldText`/`newText`.
    fn prepare_arguments(&self, mut args: Map<String, Value>) -> Map<String, Value> {
        match args.get("edits") {
            Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
                Ok(parsed @ Value::Array(_)) => {
                    args.insert("edits".into(), parsed);
                }
                Ok(parsed) if is_single_edit(&parsed) => {
                    args.insert("edits".into(), Value::Array(vec![parsed]));
                }
                _ => {}
            },
            Some(single) if is_single_edit(single) => {
                let single = single.clone();
                args.insert("edits".into(), Value::Array(vec![single]));
            }
            _ => {}
        }
        if let (Some(Value::String(old)), Some(Value::String(new))) =
            (args.get("oldText").cloned(), args.get("newText").cloned())
        {
            let mut edits = match args.get("edits") {
                Some(Value::Array(edits)) => edits.clone(),
                _ => Vec::new(),
            };
            edits.push(json!({"oldText": old, "newText": new}));
            args.shift_remove("oldText");
            args.shift_remove("newText");
            args.insert("edits".into(), Value::Array(edits));
        }
        args
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        cancel: CancellationToken,
        _updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let path = args["path"].as_str().unwrap_or_default().to_owned();
            let edits: Vec<Replacement> = args["edits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|edit| Replacement {
                    old_text: edit["oldText"].as_str().unwrap_or_default().to_owned(),
                    new_text: edit["newText"].as_str().unwrap_or_default().to_owned(),
                })
                .collect();
            if edits.is_empty() {
                return Err(
                    "Edit tool input is invalid. edits must contain at least one replacement."
                        .into(),
                );
            }
            let absolute = super::path::resolve_to_cwd(&path, &self.env.cwd);
            with_file_lock(&absolute, async {
                check_abort(&cancel)?;
                let metadata = tokio::fs::metadata(&absolute).await;
                let writable = metadata
                    .as_ref()
                    .map(|metadata| !metadata.permissions().readonly());
                match writable {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(format!("Could not edit file: {path}. Error code: EACCES."));
                    }
                    Err(err) => {
                        check_abort(&cancel)?;
                        return Err(format!("Could not edit file: {path}. {}.", error_code(err)));
                    }
                }
                let raw = tokio::fs::read(&absolute)
                    .await
                    .map_err(|err| format!("Could not edit file: {path}. {}.", error_code(&err)))?;
                check_abort(&cancel)?;
                let raw = String::from_utf8_lossy(&raw).into_owned();
                let (bom, content) = match raw.strip_prefix('\u{FEFF}') {
                    Some(rest) => ("\u{FEFF}", rest),
                    None => ("", raw.as_str()),
                };
                let ending = detect_line_ending(content);
                let normalized = normalize_to_lf(content);
                let new_content = apply_edits(&normalized, &edits, &path)?;
                check_abort(&cancel)?;
                let output = format!("{bom}{}", restore_line_endings(&new_content, ending));
                tokio::fs::write(&absolute, output)
                    .await
                    .map_err(|err| format!("Could not edit file: {path}. {}.", error_code(&err)))?;
                check_abort(&cancel)?;
                let (diff, first_changed) = display_diff(&normalized, &new_content);
                let patch = unified_patch(&path, &normalized, &new_content);
                let mut details = json!({"diff": diff, "patch": patch});
                if let Some(line) = first_changed {
                    details["firstChangedLine"] = json!(line);
                }
                Ok(text_result(
                    format!("Successfully replaced {} block(s) in {path}.", edits.len()),
                    Some(details),
                ))
            })
            .await
        })
    }
}
