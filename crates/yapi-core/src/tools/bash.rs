//! `bash`: run a command, stream its combined output, keep the tail.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_types::event::ToolResult;
use yapi_types::message::ToolDeclaration;

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, Truncation, format_size, tail_bytes,
    truncate_tail,
};
use super::{ToolEnv, declaration, text_result};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);

/// The `bash` tool.
pub struct Bash {
    env: ToolEnv,
    declaration: ToolDeclaration,
    output_schema: Value,
}

impl Bash {
    /// A `bash` tool for `env`.
    pub fn new(env: ToolEnv) -> Bash {
        Bash {
            env,
            declaration: declaration(
                "bash",
                format!(
                    "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({"type":"object","required":["command"],"properties":{
                    "command":{"type":"string","description":"Shell command to execute"},
                    "timeout":{"type":"number","description":"Timeout in seconds (optional, no default timeout)"}}}),
            ),
            // pi's `bashOutputSchema`: what scripts receive, also for non-zero exits.
            output_schema: json!({"type":"object","required":["output","truncated","exit_code","wall_time_seconds"],"properties":{
                "output":{"type":"string","description":"Combined stdout and stderr, possibly truncated"},
                "truncated":{"type":"boolean"},
                "full_output_path":{"type":"string","description":"Full output, when truncated"},
                "exit_code":{"type":"number"},
                "wall_time_seconds":{"type":"number"}}}),
        }
    }
}

/// The shell pi uses: `/bin/bash`, else `bash` on `PATH`, else `sh`.
pub fn shell() -> (String, Vec<String>) {
    if std::path::Path::new("/bin/bash").exists() {
        return ("/bin/bash".into(), vec!["-c".into()]);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("bash");
            if candidate.is_file() {
                return (candidate.to_string_lossy().into_owned(), vec!["-c".into()]);
            }
        }
    }
    ("sh".into(), vec!["-c".into()])
}

/// The shell for `!` commands: the `shellPath` setting when set, else [`shell`].
pub fn shell_with(custom: Option<&str>) -> Result<(String, Vec<String>), String> {
    match custom {
        Some(path) if std::path::Path::new(path).exists() => {
            Ok((path.to_owned(), vec!["-c".into()]))
        }
        Some(path) => Err(format!("Custom shell path not found: {path}")),
        None => Ok(shell()),
    }
}

/// A shell process for `command` in `cwd`, in its own process group, with
/// piped output, the agent's `bin_dir` leading `PATH` and pi's session
/// variables removed.
pub(crate) fn shell_command(
    (shell, shell_args): (String, Vec<String>),
    command: &str,
    cwd: &std::path::Path,
    bin_dir: &std::path::Path,
) -> tokio::process::Command {
    let mut process = tokio::process::Command::new(shell);
    process
        .envs(crate::config::child_env())
        .args(shell_args)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    #[cfg(unix)]
    process.process_group(0);
    // pi's getShellEnv: the agent's bin directory leads PATH.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    if !entries.iter().any(|entry| entry == bin_dir) {
        entries.insert(0, bin_dir.to_path_buf());
        if let Ok(joined) =
            std::env::join_paths(entries.iter().filter(|p| !p.as_os_str().is_empty()))
        {
            process.env("PATH", joined);
        }
    }
    for name in [
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
    ] {
        process.env_remove(name);
    }
    process
}

/// Collects output: a rolling tail in memory and, past the limits, the full
/// output in a temp file.
struct Accumulator {
    raw: Vec<u8>,
    pending: Vec<u8>,
    tail: String,
    tail_at_line_start: bool,
    total_bytes: usize,
    completed_lines: usize,
    open_line: bool,
    current_line_bytes: usize,
    file: Option<(PathBuf, std::fs::File)>,
}

impl Accumulator {
    fn new() -> Accumulator {
        Accumulator {
            raw: Vec::new(),
            pending: Vec::new(),
            tail: String::new(),
            tail_at_line_start: true,
            total_bytes: 0,
            completed_lines: 0,
            open_line: false,
            current_line_bytes: 0,
            file: None,
        }
    }

    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.open_line)
    }

    fn over_limits(&self) -> bool {
        self.total_bytes > DEFAULT_MAX_BYTES || self.total_lines() > DEFAULT_MAX_LINES
    }

    fn append(&mut self, data: &[u8]) {
        use std::io::Write;
        let decoded = yapi_types::js::decode_utf8_stream(&mut self.pending, data);
        self.append_text(&decoded);
        if self.file.is_some() || self.over_limits() {
            self.spill();
            if let Some((_, file)) = &mut self.file {
                let _ = file.write_all(data);
            }
        } else {
            self.raw.extend_from_slice(data);
        }
    }

    fn finish(&mut self) {
        if !self.pending.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            self.append_text(&rest);
        }
        if self.over_limits() {
            self.spill();
        }
    }

    fn append_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_bytes += text.len();
        self.tail.push_str(text);
        let rolling = DEFAULT_MAX_BYTES * 2;
        if self.tail.len() > rolling * 2 {
            let kept = tail_bytes(&self.tail, rolling);
            let start = self.tail.len() - kept.len();
            self.tail_at_line_start = start == 0 || self.tail.as_bytes()[start - 1] == b'\n';
            self.tail = kept.to_owned();
        }
        match text.rfind('\n') {
            None => {
                self.current_line_bytes += text.len();
                self.open_line = true;
            }
            Some(last) => {
                self.completed_lines += text.matches('\n').count();
                self.current_line_bytes = text.len() - last - 1;
                self.open_line = last + 1 < text.len();
            }
        }
    }

    fn spill(&mut self) {
        use std::io::Write;
        if self.file.is_some() {
            return;
        }
        let path =
            std::env::temp_dir().join(format!("yapi-bash-{}.log", crate::time::random_hex(8)));
        if let Ok(mut file) = std::fs::File::create(&path) {
            let _ = file.write_all(&std::mem::take(&mut self.raw));
            self.file = Some((path, file));
        }
    }

    fn snapshot(&mut self, persist: bool) -> (Truncation, Option<PathBuf>) {
        let text = if self.tail_at_line_start {
            self.tail.as_str()
        } else {
            self.tail
                .find('\n')
                .map_or(self.tail.as_str(), |index| &self.tail[index + 1..])
        };
        let mut truncation = truncate_tail(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let truncated = self.over_limits();
        truncation.truncated = truncated;
        truncation.truncated_by = if truncated {
            truncation
                .truncated_by
                .or(Some(if self.total_bytes > DEFAULT_MAX_BYTES {
                    TruncatedBy::Bytes
                } else {
                    TruncatedBy::Lines
                }))
        } else {
            None
        };
        truncation.total_lines = self.total_lines();
        truncation.total_bytes = self.total_bytes;
        if persist && truncated {
            self.spill();
        }
        (truncation, self.file.as_ref().map(|(path, _)| path.clone()))
    }

    fn full_output(&self) -> (String, bool) {
        let Some((path, _)) = &self.file else {
            return (String::from_utf8_lossy(&self.raw).into_owned(), false);
        };
        let Ok(bytes) = std::fs::read(path) else {
            return (String::new(), false);
        };
        if bytes.len() <= STRUCTURED_OUTPUT_MAX_BYTES {
            return (String::from_utf8_lossy(&bytes).into_owned(), false);
        }
        let head_len = STRUCTURED_OUTPUT_MAX_BYTES / 2;
        let tail_len = STRUCTURED_OUTPUT_MAX_BYTES - head_len;
        let mut head_end = head_len;
        while head_end > 0 && (bytes[head_end] & 0xc0) == 0x80 {
            head_end -= 1;
        }
        let mut tail_start = bytes.len() - tail_len;
        while tail_start < bytes.len() && (bytes[tail_start] & 0xc0) == 0x80 {
            tail_start += 1;
        }
        let omitted = bytes.len() - head_len - tail_len;
        (
            format!(
                "{}\n\n[... {omitted} bytes omitted ...]\n\n{}",
                String::from_utf8_lossy(&bytes[..head_end]),
                String::from_utf8_lossy(&bytes[tail_start..])
            ),
            true,
        )
    }
}

fn format_output(
    truncation: &Truncation,
    path: &Option<PathBuf>,
    last_line_bytes: usize,
    empty: &str,
) -> (String, Option<Value>) {
    let mut text = if truncation.content.is_empty() {
        empty.to_owned()
    } else {
        truncation.content.clone()
    };
    if !truncation.truncated {
        return (text, None);
    }
    let path_text = path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let start = truncation.total_lines - truncation.output_lines + 1;
    let end = truncation.total_lines;
    if truncation.last_line_partial {
        text += &format!(
            "\n\n[Showing last {} of line {end} (line is {}). Full output: {path_text}]",
            format_size(truncation.output_bytes),
            format_size(last_line_bytes)
        );
    } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
        text += &format!(
            "\n\n[Showing lines {start}-{end} of {}. Full output: {path_text}]",
            truncation.total_lines
        );
    } else {
        text += &format!(
            "\n\n[Showing lines {start}-{end} of {} ({} limit). Full output: {path_text}]",
            truncation.total_lines,
            format_size(DEFAULT_MAX_BYTES)
        );
    }
    let mut details = json!({ "truncation": truncation });
    if let Some(path) = path {
        details["fullOutputPath"] = json!(path.display().to_string());
    }
    (text, Some(details))
}

/// A progress update with the output so far.
fn progress(truncation: &Truncation, path: Option<&Path>) -> ToolResult {
    let mut details = json!({});
    if truncation.truncated {
        details["truncation"] = json!(truncation);
    }
    if let Some(path) = path {
        details["fullOutputPath"] = json!(path.display().to_string());
    }
    text_result(truncation.content.clone(), Some(details))
}

fn with_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_owned()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// Process groups of running commands: pi's tracked detached children,
/// killed when yapi is terminated.
static TRACKED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Keeps a command's process group tracked until it is dropped.
pub(crate) struct TrackedChild(Option<u32>);

impl TrackedChild {
    /// Tracks the process group led by `pid`.
    pub(crate) fn new(pid: Option<u32>) -> TrackedChild {
        if let Some(pid) = pid {
            TRACKED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(pid);
        }
        TrackedChild(pid)
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            TRACKED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|tracked| *tracked != pid);
        }
    }
}

/// Kills every running command's process tree, as pi's
/// `killTrackedDetachedChildren` does before exiting on a signal.
pub fn kill_tracked_children() {
    let tracked = std::mem::take(
        &mut *TRACKED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for pid in tracked {
        kill_tree(Some(pid));
    }
}

/// Sends `signal` to the process group of `pid`, or to `pid` alone.
#[cfg(unix)]
pub(crate) fn signal_tree(pid: Option<u32>, signal: rustix::process::Signal) {
    use rustix::process::{Pid, kill_process, kill_process_group};
    let Some(pid) = pid.and_then(|pid| Pid::from_raw(pid as i32)) else {
        return;
    };
    if kill_process_group(pid, signal).is_err() {
        let _ = kill_process(pid, signal);
    }
}

#[cfg(unix)]
pub(crate) fn kill_tree(pid: Option<u32>) {
    signal_tree(pid, rustix::process::Signal::KILL);
}

#[cfg(not(unix))]
pub(crate) fn kill_tree(_pid: Option<u32>) {}

enum Ending {
    Exited(Option<i32>),
    Aborted,
    TimedOut,
}

impl Tool for Bash {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&self.output_schema)
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        cancel: CancellationToken,
        updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let command = args["command"].as_str().unwrap_or_default().to_owned();
            let timeout = match args.get("timeout").and_then(Value::as_f64) {
                None => None,
                Some(seconds) if !seconds.is_finite() || seconds <= 0.0 => {
                    return Err("Invalid timeout: must be a finite number of seconds".into());
                }
                Some(seconds) if seconds * 1000.0 > MAX_TIMEOUT_MS => {
                    return Err(format!(
                        "Invalid timeout: maximum is {} seconds",
                        MAX_TIMEOUT_MS / 1000.0
                    ));
                }
                Some(seconds) => Some(seconds),
            };
            let cwd = &self.env.cwd;
            if cancel.is_cancelled() {
                return Err("Command aborted".into());
            }
            if !cwd.exists() {
                return Err(format!(
                    "Working directory does not exist: {}\nCannot execute bash commands.",
                    cwd.display()
                ));
            }

            let mut process = shell_command(shell(), &command, cwd, &self.env.bin_dir);
            let runtime = self.env.runtime();
            if let Some(id) = &runtime.session_id {
                process.env("PI_SESSION_ID", id);
            }
            if let Some(file) = &runtime.session_file {
                process.env("PI_SESSION_FILE", file);
            }
            if let Some(model) = &runtime.model {
                process.env("PI_PROVIDER", &model.provider);
                process.env("PI_MODEL", &model.id);
            }
            if let Some(level) = runtime.thinking_level {
                process.env("PI_REASONING_LEVEL", level.as_str());
            }
            let mut child = process.spawn().map_err(|err| err.to_string())?;
            let pid = child.id();
            let _tracked = TrackedChild::new(pid);
            let mut stdout = child.stdout.take();
            let mut stderr = child.stderr.take();

            updates(ToolResult::default());
            let started = Instant::now();
            let mut output = Accumulator::new();
            let mut last_update = Instant::now() - UPDATE_THROTTLE;
            let mut dirty = false;
            let deadline = timeout.map(|seconds| started + Duration::from_secs_f64(seconds));
            let mut ending: Option<Ending> = None;
            let mut exited_at: Option<Instant> = None;
            let mut exit_code = None;
            let (mut out_buf, mut err_buf) = (vec![0u8; 8192], vec![0u8; 8192]);

            loop {
                if stdout.is_none() && stderr.is_none() && exited_at.is_some() {
                    break;
                }
                let grace = exited_at.map(|at| at + EXIT_STDIO_GRACE);
                let next_update = dirty.then(|| last_update + UPDATE_THROTTLE);
                // Biased: when both pipes have data, stdout's came first more often
                // than not, and a random pick would reorder `echo a; echo b >&2`.
                tokio::select! {
                    biased;
                    read = async { stdout.as_mut().unwrap_or_else(|| unreachable!()).read(&mut out_buf).await }, if stdout.is_some() => {
                        match read {
                            Ok(0) | Err(_) => stdout = None,
                            Ok(n) => { output.append(&out_buf[..n]); dirty = true; if let Some(at) = &mut exited_at { *at = Instant::now(); } }
                        }
                    }
                    read = async { stderr.as_mut().unwrap_or_else(|| unreachable!()).read(&mut err_buf).await }, if stderr.is_some() => {
                        match read {
                            Ok(0) | Err(_) => stderr = None,
                            Ok(n) => { output.append(&err_buf[..n]); dirty = true; if let Some(at) = &mut exited_at { *at = Instant::now(); } }
                        }
                    }
                    status = child.wait(), if exited_at.is_none() => {
                        exit_code = status.ok().and_then(|status| {
                            #[cfg(unix)]
                            {
                                use std::os::unix::process::ExitStatusExt;
                                status.code().or_else(|| status.signal().map(|signal| 128 + signal))
                            }
                            #[cfg(not(unix))]
                            { status.code() }
                        });
                        exited_at = Some(Instant::now());
                    }
                    () = async { tokio::time::sleep_until(grace.unwrap_or_else(|| unreachable!()).into()).await }, if grace.is_some() => {
                        break;
                    }
                    () = cancel.cancelled(), if ending.is_none() => {
                        ending = Some(Ending::Aborted);
                        kill_tree(pid);
                    }
                    () = async { tokio::time::sleep_until(deadline.unwrap_or_else(|| unreachable!()).into()).await }, if deadline.is_some() && ending.is_none() => {
                        ending = Some(Ending::TimedOut);
                        kill_tree(pid);
                    }
                    () = async { tokio::time::sleep_until(next_update.unwrap_or_else(|| unreachable!()).into()).await }, if next_update.is_some() => {
                        dirty = false;
                        last_update = Instant::now();
                        let (truncation, path) = output.snapshot(true);
                        updates(progress(&truncation, path.as_deref()));
                    }
                }
            }
            output.finish();
            let (truncation, path) = output.snapshot(true);
            // pi's finishOutput sends output still waiting for the throttle.
            if dirty {
                updates(progress(&truncation, path.as_deref()));
            }
            let ending = ending.unwrap_or(Ending::Exited(exit_code));
            match ending {
                Ending::Aborted => {
                    let (text, _) =
                        format_output(&truncation, &path, output.current_line_bytes, "");
                    Err(with_status(&text, "Command aborted"))
                }
                Ending::TimedOut => {
                    let (text, _) =
                        format_output(&truncation, &path, output.current_line_bytes, "");
                    let seconds = yapi_types::json::to_string(&timeout.unwrap_or_default())
                        .unwrap_or_default();
                    Err(with_status(
                        &text,
                        &format!("Command timed out after {seconds} seconds"),
                    ))
                }
                Ending::Exited(code) => {
                    let (text, details) =
                        format_output(&truncation, &path, output.current_line_bytes, "(no output)");
                    let Some(code) = code else {
                        return Err(with_status(
                            &text,
                            "Command terminated without an exit code",
                        ));
                    };
                    let wall = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
                    let (full, full_truncated) = output.full_output();
                    let mut structured = json!({"output": full, "truncated": full_truncated});
                    if full_truncated && let Some(path) = &path {
                        structured["full_output_path"] = json!(path.display().to_string());
                    }
                    structured["exit_code"] = json!(code);
                    structured["wall_time_seconds"] = json!(wall);
                    let mut result = text_result(
                        if code == 0 {
                            text
                        } else {
                            with_status(&text, &format!("Command exited with code {code}"))
                        },
                        details,
                    );
                    result.structured_content = Some(structured);
                    if code != 0 {
                        result.is_error = Some(true);
                    }
                    Ok(result)
                }
            }
        })
    }
}
