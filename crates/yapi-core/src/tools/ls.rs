//! `ls`: directory entries, sorted, with `/` after directories.

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_types::event::ToolResult;
use yapi_types::message::ToolDeclaration;

use super::path::resolve_to_cwd;
use super::truncate::DEFAULT_MAX_BYTES;
use super::{ToolEnv, capped_output, js_number, node_error, plain_declaration, text_result};
use yapi_types::collate::locale_compare;

const DEFAULT_LIMIT: f64 = 500.0;

/// The `ls` tool.
pub struct Ls {
    env: ToolEnv,
    declaration: ToolDeclaration,
}

impl Ls {
    /// An `ls` tool for `env`.
    pub fn new(env: ToolEnv) -> Ls {
        Ls {
            env,
            declaration: plain_declaration(
                "ls",
                format!(
                    "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {} entries or {}KB (whichever is hit first).",
                    DEFAULT_LIMIT,
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({"type":"object","properties":{
                    "path":{"type":"string","description":"Directory to list (default: current directory)"},
                    "limit":{"type":"number","description":"Maximum number of entries to return (default: 500)"}}}),
            ),
        }
    }
}

impl Tool for Ls {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        cancel: CancellationToken,
        _updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            self.list(args)
        })
    }
}

impl Ls {
    fn list(&self, args: Value) -> Result<ToolResult, String> {
        let dir = args["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or(".");
        let path = resolve_to_cwd(dir, &self.env.cwd);
        let limit = args["limit"].as_f64().unwrap_or(DEFAULT_LIMIT);
        if !path.exists() {
            return Err(format!("Path not found: {}", path.display()));
        }
        let metadata = std::fs::metadata(&path).map_err(|err| node_error(&err, "stat", &path))?;
        if !metadata.is_dir() {
            return Err(format!("Not a directory: {}", path.display()));
        }
        let mut entries: Vec<String> = std::fs::read_dir(&path)
            .map_err(|err| {
                format!(
                    "Cannot read directory: {}",
                    node_error(&err, "scandir", &path)
                )
            })?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort_by(|a, b| locale_compare(&a.to_lowercase(), &b.to_lowercase()));
        let mut results = Vec::new();
        let mut limit_reached = false;
        for entry in entries {
            if results.len() as f64 >= limit {
                limit_reached = true;
                break;
            }
            // Entries that cannot be stat'ed, such as broken links, are skipped.
            let Ok(metadata) = std::fs::metadata(path.join(&entry)) else {
                continue;
            };
            results.push(if metadata.is_dir() {
                format!("{entry}/")
            } else {
                entry
            });
        }
        if results.is_empty() {
            return Ok(text_result("(empty directory)", None));
        }
        let mut details = Map::new();
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(format!(
                "{} entries limit reached. Use limit={} for more",
                js_number(limit),
                js_number(limit * 2.0)
            ));
            details.insert("entryLimitReached".into(), json!(limit));
        }
        let text = capped_output(&results.join("\n"), notices, Vec::new(), &mut details);
        Ok(text_result(
            text,
            (!details.is_empty()).then_some(Value::Object(details)),
        ))
    }
}
