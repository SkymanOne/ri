//! `grep`: ripgrep over files, with context lines and pi's output limits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::io::AsyncBufReadExt;
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_types::event::ToolResult;
use yapi_types::message::ToolDeclaration;

use super::external::{ExternalTool, ensure_tool};
use super::path::{relative, resolve_to_cwd};
use super::truncate::{DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, truncate_line};
use super::{ToolEnv, capped_output, js_number, plain_declaration, text_result};

const DEFAULT_LIMIT: f64 = 100.0;

/// The `grep` tool.
pub struct Grep {
    env: ToolEnv,
    declaration: ToolDeclaration,
}

impl Grep {
    /// A `grep` tool for `env`.
    pub fn new(env: ToolEnv) -> Grep {
        Grep {
            env,
            declaration: plain_declaration(
                "grep",
                format!(
                    "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {} matches or {}KB (whichever is hit first). Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
                    DEFAULT_LIMIT,
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({"type":"object","required":["pattern"],"properties":{
                    "pattern":{"type":"string","description":"Search pattern (regex or literal string)"},
                    "path":{"type":"string","description":"Directory or file to search (default: current directory)"},
                    "glob":{"type":"string","description":"Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},
                    "ignoreCase":{"type":"boolean","description":"Case-insensitive search (default: false)"},
                    "literal":{"type":"boolean","description":"Treat pattern as literal string instead of regex (default: false)"},
                    "context":{"type":"number","description":"Number of lines to show before and after each match (default: 0)"},
                    "limit":{"type":"number","description":"Maximum number of matches to return (default: 100)"}}}),
            ),
        }
    }
}

impl Tool for Grep {
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
            self.search(args, cancel).await
        })
    }
}

struct Match {
    file: String,
    line: u64,
    text: Option<String>,
}

/// Lines of a file with `\r\n` and `\r` as line ends; empty when unreadable.
fn file_lines(path: &Path) -> Vec<String> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

impl Grep {
    async fn search(&self, args: Value, cancel: CancellationToken) -> Result<ToolResult, String> {
        let Some(rg) = ensure_tool(ExternalTool::Rg, &self.env.bin_dir).await else {
            return Err("ripgrep (rg) is not available and could not be downloaded".into());
        };
        let pattern = args["pattern"].as_str().unwrap_or_default();
        let dir = args["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or(".");
        let search = resolve_to_cwd(dir, &self.env.cwd);
        let is_directory = match std::fs::metadata(&search) {
            Ok(metadata) => metadata.is_dir(),
            Err(_) => return Err(format!("Path not found: {}", search.display())),
        };
        let context = args["context"].as_f64().filter(|c| *c > 0.0).unwrap_or(0.0) as u64;
        let limit = args["limit"].as_f64().unwrap_or(DEFAULT_LIMIT).max(1.0);
        let format_path = |file: &str| -> String {
            let file = Path::new(file);
            if is_directory {
                let relative = relative(&search, file);
                if !relative.is_empty() && !relative.starts_with("..") {
                    return relative.replace('\\', "/");
                }
            }
            file.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };

        let mut command = tokio::process::Command::new(&rg);
        command.args(["--json", "--line-number", "--color=never", "--hidden"]);
        if args["ignoreCase"] == true {
            command.arg("--ignore-case");
        }
        if args["literal"] == true {
            command.arg("--fixed-strings");
        }
        if let Some(glob) = args["glob"].as_str().filter(|g| !g.is_empty()) {
            command.args(["--glob", glob]);
        }
        command
            .arg("--")
            .arg(pattern)
            .arg(&search)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|err| format!("Failed to run ripgrep: {err}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut text = String::new();
            if let Some(mut stderr) = stderr {
                use tokio::io::AsyncReadExt;
                let mut bytes = Vec::new();
                let _ = stderr.read_to_end(&mut bytes).await;
                text = String::from_utf8_lossy(&bytes).into_owned();
            }
            text
        });

        let mut matches: Vec<Match> = Vec::new();
        let mut limit_reached = false;
        if let Some(stdout) = stdout {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                let line = tokio::select! {
                    () = cancel.cancelled() => {
                        let _ = child.kill().await;
                        return Err("Operation aborted".into());
                    }
                    line = lines.next_line() => line,
                };
                let Ok(Some(line)) = line else { break };
                if line.trim().is_empty() || matches.len() as f64 >= limit {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if event["type"] != "match" {
                    continue;
                }
                let data = &event["data"];
                if let (Some(file), Some(number)) =
                    (data["path"]["text"].as_str(), data["line_number"].as_u64())
                {
                    matches.push(Match {
                        file: file.to_owned(),
                        line: number,
                        text: data["lines"]["text"].as_str().map(str::to_owned),
                    });
                }
                if matches.len() as f64 >= limit {
                    limit_reached = true;
                    let _ = child.start_kill();
                }
            }
        }
        let status = child.wait().await;
        let stderr = stderr_task.await.unwrap_or_default();
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        if !limit_reached {
            let code = status.ok().and_then(|status| status.code());
            if !matches!(code, Some(0 | 1)) {
                let message = stderr.trim();
                return Err(if message.is_empty() {
                    format!(
                        "ripgrep exited with code {}",
                        code.map_or("null".to_owned(), |c| c.to_string())
                    )
                } else {
                    message.to_owned()
                });
            }
        }
        if matches.is_empty() {
            return Ok(text_result("No matches found", None));
        }

        let mut lines_truncated = false;
        let mut cache: HashMap<String, Vec<String>> = HashMap::new();
        let mut output: Vec<String> = Vec::new();
        for found in &matches {
            let shown = format_path(&found.file);
            match (&found.text, context) {
                (Some(text), 0) => {
                    let text = text.replace("\r\n", "\n").replace('\r', "");
                    let text = text.strip_suffix('\n').unwrap_or(&text);
                    let (text, cut) = truncate_line(text, GREP_MAX_LINE_LENGTH);
                    lines_truncated |= cut;
                    output.push(format!("{shown}:{}: {text}", found.line));
                }
                _ => {
                    let lines = cache
                        .entry(found.file.clone())
                        .or_insert_with(|| file_lines(&PathBuf::from(&found.file)));
                    if lines.is_empty() {
                        output.push(format!("{shown}:{}: (unable to read file)", found.line));
                        continue;
                    }
                    let (start, end) = if context > 0 {
                        (
                            found.line.saturating_sub(context).max(1),
                            (found.line + context).min(lines.len() as u64),
                        )
                    } else {
                        (found.line, found.line)
                    };
                    for current in start..=end {
                        let text = lines
                            .get(current as usize - 1)
                            .map(|line| line.replace('\r', ""))
                            .unwrap_or_default();
                        let (text, cut) = truncate_line(&text, GREP_MAX_LINE_LENGTH);
                        lines_truncated |= cut;
                        if current == found.line {
                            output.push(format!("{shown}:{current}: {text}"));
                        } else {
                            output.push(format!("{shown}-{current}- {text}"));
                        }
                    }
                }
            }
        }

        let mut details = Map::new();
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(format!(
                "{} matches limit reached. Use limit={} for more, or refine pattern",
                js_number(limit),
                js_number(limit * 2.0)
            ));
            details.insert("matchLimitReached".into(), json!(limit));
        }
        let after = if lines_truncated {
            vec![format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            )]
        } else {
            Vec::new()
        };
        let text = capped_output(&output.join("\n"), notices, after, &mut details);
        if lines_truncated {
            details.insert("linesTruncated".into(), json!(true));
        }
        Ok(text_result(
            text,
            (!details.is_empty()).then_some(Value::Object(details)),
        ))
    }
}
