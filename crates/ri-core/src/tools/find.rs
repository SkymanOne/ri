//! `find`: files by glob pattern through `fd`, respecting `.gitignore`.

use std::path::Path;
use std::process::Stdio;

use futures_util::future::BoxFuture;
use ri_agent::{Tool, UpdateSink};
use ri_types::event::ToolResult;
use ri_types::message::ToolDeclaration;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::external::{ExternalTool, ensure_tool};
use super::path::{relative, resolve_to_cwd};
use super::truncate::DEFAULT_MAX_BYTES;
use super::{ToolEnv, capped_output, js_number, plain_declaration, text_result};

const DEFAULT_LIMIT: f64 = 1000.0;

/// The `find` tool.
pub struct Find {
    env: ToolEnv,
    declaration: ToolDeclaration,
}

impl Find {
    /// A `find` tool for `env`.
    pub fn new(env: ToolEnv) -> Find {
        Find {
            env,
            declaration: plain_declaration(
                "find",
                format!(
                    "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {} results or {}KB (whichever is hit first).",
                    DEFAULT_LIMIT,
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({"type":"object","required":["pattern"],"properties":{
                    "pattern":{"type":"string","description":"Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},
                    "path":{"type":"string","description":"Directory to search in (default: current directory)"},
                    "limit":{"type":"number","description":"Maximum number of results (default: 1000)"}}}),
            ),
        }
    }
}

impl Tool for Find {
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
            tokio::select! {
                () = cancel.cancelled() => Err("Operation aborted".to_owned()),
                result = self.find(args) => result,
            }
        })
    }
}

/// A result path relative to the search root with `/` separators, keeping a
/// trailing separator.
fn relativize(result: &str, search: &Path) -> String {
    let trailing = result.ends_with('/');
    let path = Path::new(result);
    let relative = if path.is_absolute() {
        relative(search, path)
    } else {
        result.to_owned()
    };
    if trailing && !relative.ends_with('/') {
        format!("{relative}/")
    } else {
        relative
    }
}

impl Find {
    async fn find(&self, args: Value) -> Result<ToolResult, String> {
        let pattern = args["pattern"].as_str().unwrap_or_default();
        let dir = args["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or(".");
        let search = resolve_to_cwd(dir, &self.env.cwd);
        let limit = args["limit"].as_f64().unwrap_or(DEFAULT_LIMIT);
        let Some(fd) = ensure_tool(ExternalTool::Fd, &self.env.bin_dir).await else {
            return Err("fd is not available and could not be downloaded".into());
        };
        let mut command = tokio::process::Command::new(&fd);
        command.args(["--glob", "--color=never", "--hidden"]);
        // Outside a git repository fd would ignore .gitignore files; inside one, its
        // own rules stop parent rules at nested repositories.
        if !search.ancestors().any(|dir| dir.join(".git").exists()) {
            command.arg("--no-require-git");
        }
        command.args(["--max-results", &js_number(limit)]);
        // With a path in the pattern, fd matches the absolute path.
        let mut effective = pattern.to_owned();
        if pattern.contains('/') {
            command.arg("--full-path");
            if !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" {
                effective = format!("**/{pattern}");
            }
        }
        let output = command
            .arg("--")
            .arg(&effective)
            .arg(&search)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|err| format!("Failed to run fd: {err}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        let joined = lines.join("\n");
        if !output.status.success() && joined.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(if message.is_empty() {
                format!(
                    "fd exited with code {}",
                    output
                        .status
                        .code()
                        .map_or("null".to_owned(), |c| c.to_string())
                )
            } else {
                message.to_owned()
            });
        }
        if joined.is_empty() {
            return Ok(text_result("No files found matching pattern", None));
        }
        let results: Vec<String> = lines
            .iter()
            .map(|line| line.trim_end_matches('\r').trim())
            .filter(|line| !line.is_empty())
            .map(|line| relativize(line, &search))
            .collect();
        let mut details = Map::new();
        let mut notices = Vec::new();
        if results.len() as f64 >= limit {
            notices.push(format!(
                "{} results limit reached. Use limit={} for more, or refine pattern",
                js_number(limit),
                js_number(limit * 2.0)
            ));
            details.insert("resultLimitReached".into(), json!(limit));
        }
        let text = capped_output(&results.join("\n"), notices, Vec::new(), &mut details);
        Ok(text_result(
            text,
            (!details.is_empty()).then_some(Value::Object(details)),
        ))
    }
}
