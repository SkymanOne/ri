//! User `!` commands: run in the shell, stream sanitized output, keep the tail.
//!
//! Port of `packages/coding-agent/src/core/bash-executor.ts` in pi `v1.0.0`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex_lite::Regex;
use tokio_util::sync::CancellationToken;

use crate::tools::bash::{Pipes, create_log, exit_code_of, kill_tree, shell_command, shell_with};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);

pub use yapi_types::rpc::BashResult;

static ANSI: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // OSC up to the first string terminator (BEL, ESC \ or 0x9c), or CSI.
    Regex::new(concat!(
        r"(?:\x1b\][\s\S]*?(?:\x07|\x1b\\|\x{9c}))",
        r"|[\x1b\x{9b}][\[\]()#;?]*(?:\d{1,4}(?:[;:]\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]",
    ))
    .ok()
});

/// pi's `stripAnsi`: removes OSC and CSI sequences.
pub fn strip_ansi(text: &str) -> String {
    if !text.contains(['\x1b', '\u{9b}']) {
        return text.to_owned();
    }
    match ANSI.as_ref() {
        Some(regex) => regex.replace_all(text, "").into_owned(),
        None => text.to_owned(),
    }
}

/// pi's `sanitizeBinaryOutput`: drops control characters other than tab,
/// newline and carriage return, and Unicode interlinear annotations.
pub fn sanitize_binary(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !matches!(c, '\x00'..='\x08' | '\x0b' | '\x0c' | '\x0e'..='\x1f' | '\u{fff9}'..='\u{fffb}')
        })
        .collect()
}

struct Output {
    chunks: Vec<String>,
    kept: usize,
    total_bytes: usize,
    pending: Vec<u8>,
    file: Option<(PathBuf, std::fs::File)>,
}

impl Output {
    fn ensure_file(&mut self) {
        if self.file.is_some() {
            return;
        }
        if let Some((path, mut file)) = create_log() {
            for chunk in &self.chunks {
                let _ = file.write_all(chunk.as_bytes());
            }
            self.file = Some((path, file));
        }
    }

    fn push(&mut self, data: &[u8], on_chunk: &mut impl FnMut(&str)) {
        self.total_bytes += data.len();
        let decoded = yapi_types::js::decode_utf8_stream(&mut self.pending, data);
        self.text(&decoded, on_chunk);
    }

    fn text(&mut self, decoded: &str, on_chunk: &mut impl FnMut(&str)) {
        let text = sanitize_binary(&strip_ansi(decoded)).replace('\r', "");
        if self.total_bytes > DEFAULT_MAX_BYTES {
            self.ensure_file();
        }
        if let Some((_, file)) = &mut self.file {
            let _ = file.write_all(text.as_bytes());
        }
        self.kept += text.encode_utf16().count();
        self.chunks.push(text.clone());
        while self.kept > DEFAULT_MAX_BYTES * 2 && self.chunks.len() > 1 {
            let removed = self.chunks.remove(0);
            self.kept -= removed.encode_utf16().count();
        }
        on_chunk(&text);
    }

    fn finish(mut self, exit_code: Option<i32>, cancelled: bool) -> BashResult {
        let full = self.chunks.concat();
        let truncation = truncate_tail(&full, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        if truncation.truncated {
            self.ensure_file();
        }
        BashResult {
            output: if truncation.truncated {
                truncation.content
            } else {
                full
            },
            exit_code: if cancelled { None } else { exit_code },
            cancelled,
            truncated: truncation.truncated,
            full_output_path: self.file.map(|(path, _)| path.display().to_string()),
        }
    }
}

/// Runs `command` in `cwd` with the configured shell, calling `on_chunk` with
/// each sanitized piece of output, until it exits or `cancel` fires.
pub async fn execute(
    command: &str,
    cwd: &Path,
    shell_path: Option<&str>,
    bin_dir: &Path,
    cancel: CancellationToken,
    mut on_chunk: impl FnMut(&str),
) -> Result<BashResult, String> {
    if cancel.is_cancelled() {
        return Err("aborted".into());
    }
    let shell = shell_with(shell_path)?;
    if !cwd.exists() {
        return Err(format!(
            "Working directory does not exist: {}\nCannot execute bash commands.",
            cwd.display()
        ));
    }
    let mut child = shell_command(shell, command, cwd, bin_dir)
        .spawn()
        .map_err(|err| err.to_string())?;
    let pid = child.id();
    let _tracked = crate::tools::bash::TrackedChild::new(pid);
    let mut pipes = Pipes::take(&mut child);
    let mut output = Output {
        chunks: Vec::new(),
        kept: 0,
        total_bytes: 0,
        pending: Vec::new(),
        file: None,
    };
    let mut cancelled = false;
    let mut exit_code: Option<i32> = None;
    // Descendants may hold the pipes open: stop reading shortly after the
    // shell exits once output goes quiet.
    let mut exited_at: Option<Instant> = None;
    loop {
        if exited_at.is_some() && !pipes.is_open() {
            break;
        }
        let grace = exited_at.map(|at| at + EXIT_STDIO_GRACE);
        tokio::select! {
            biased;
            bytes = pipes.read(), if pipes.is_open() => {
                if !bytes.is_empty() {
                    output.push(bytes, &mut on_chunk);
                    if let Some(at) = &mut exited_at { *at = Instant::now(); }
                }
            }
            status = child.wait(), if exited_at.is_none() => {
                exited_at = Some(Instant::now());
                exit_code = status.ok().map(|status| exit_code_of(status).unwrap_or(1));
            }
            () = crate::tools::sleep_until_opt(grace) => {
                break;
            }
            () = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                kill_tree(pid);
            }
        }
    }
    if !output.pending.is_empty() {
        let rest = String::from_utf8_lossy(&std::mem::take(&mut output.pending)).into_owned();
        output.text(&rest, &mut on_chunk);
    }
    Ok(output.finish(exit_code, cancelled))
}

/// pi's `executeBashWithOperations`: runs a command through `exec`, which
/// sends its output to the sender it gets and returns the exit code, and
/// collects the output as [`execute`] does. A run that fails or ends after
/// `cancel` fired is cancelled.
pub async fn execute_with<F>(
    exec: impl FnOnce(tokio::sync::mpsc::UnboundedSender<Vec<u8>>) -> F,
    cancel: &CancellationToken,
    mut on_chunk: impl FnMut(&str),
) -> Result<BashResult, String>
where
    F: std::future::Future<Output = Result<Option<i32>, String>>,
{
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut output = Output {
        chunks: Vec::new(),
        kept: 0,
        total_bytes: 0,
        pending: Vec::new(),
        file: None,
    };
    let run = exec(sender);
    tokio::pin!(run);
    let outcome = loop {
        tokio::select! {
            biased;
            Some(bytes) = receiver.recv() => output.push(&bytes, &mut on_chunk),
            outcome = &mut run => break outcome,
        }
    };
    while let Ok(bytes) = receiver.try_recv() {
        output.push(&bytes, &mut on_chunk);
    }
    let cancelled = cancel.is_cancelled();
    match outcome {
        Ok(exit_code) => Ok(output.finish(exit_code, cancelled)),
        Err(_) if cancelled => Ok(output.finish(None, true)),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes_and_controls() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b]8;;http://x\x07link\x1b]8;;\x07"), "link");
        assert_eq!(sanitize_binary("a\x00b\tc\x07"), "ab\tc");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_and_cancels() {
        let dir = std::env::temp_dir();
        let mut seen = String::new();
        let result = execute(
            "printf 'one\\r\\ntwo\\n'; exit 3",
            &dir,
            None,
            &dir,
            CancellationToken::new(),
            |chunk| seen.push_str(chunk),
        )
        .await
        .unwrap_or_default();
        assert_eq!(result.output, "one\ntwo\n");
        assert_eq!(result.exit_code, Some(3));
        assert_eq!(seen, "one\ntwo\n");

        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let result = execute("sleep 5", &dir, None, &dir, cancel, |_| {})
            .await
            .unwrap_or_default();
        assert!(result.cancelled);
        assert_eq!(result.exit_code, None);
    }
}
