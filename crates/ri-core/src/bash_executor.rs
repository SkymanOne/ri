//! User `!` commands: run in the shell, stream sanitized output, keep the tail.
//!
//! Port of `packages/coding-agent/src/core/bash-executor.ts` in pi `v1.0.0`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex_lite::Regex;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::tools::bash::{kill_tree, shell_command, shell_with};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);

pub use ri_types::rpc::BashResult;

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
        let path =
            std::env::temp_dir().join(format!("ri-bash-{}.log", crate::tools::random_hex(8)));
        if let Ok(mut file) = std::fs::File::create(&path) {
            for chunk in &self.chunks {
                let _ = file.write_all(chunk.as_bytes());
            }
            self.file = Some((path, file));
        }
    }

    fn push(&mut self, data: &[u8], on_chunk: &mut impl FnMut(&str)) {
        self.total_bytes += data.len();
        self.pending.extend_from_slice(data);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(text) => text.len(),
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            Err(_) => self.pending.len(),
        };
        let rest = self.pending.split_off(valid);
        let decoded =
            String::from_utf8_lossy(&std::mem::replace(&mut self.pending, rest)).into_owned();
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
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut output = Output {
        chunks: Vec::new(),
        kept: 0,
        total_bytes: 0,
        pending: Vec::new(),
        file: None,
    };
    let (mut out_buf, mut err_buf) = (vec![0u8; 8192], vec![0u8; 8192]);
    let mut cancelled = false;
    let mut exit_code: Option<i32> = None;
    // Descendants may hold the pipes open: stop reading shortly after the
    // shell exits once output goes quiet.
    let mut exited_at: Option<Instant> = None;
    loop {
        if exited_at.is_some() && stdout.is_none() && stderr.is_none() {
            break;
        }
        let grace = exited_at.map(|at| at + EXIT_STDIO_GRACE);
        // Biased: when both pipes have data, stdout's came first more often
        // than not, and a random pick would reorder `echo a; echo b >&2`.
        tokio::select! {
            biased;
            read = async { stdout.as_mut().unwrap_or_else(|| unreachable!()).read(&mut out_buf).await }, if stdout.is_some() => {
                match read {
                    Ok(0) | Err(_) => stdout = None,
                    Ok(n) => {
                        output.push(&out_buf[..n], &mut on_chunk);
                        if let Some(at) = &mut exited_at { *at = Instant::now(); }
                    }
                }
            }
            read = async { stderr.as_mut().unwrap_or_else(|| unreachable!()).read(&mut err_buf).await }, if stderr.is_some() => {
                match read {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(n) => {
                        output.push(&err_buf[..n], &mut on_chunk);
                        if let Some(at) = &mut exited_at { *at = Instant::now(); }
                    }
                }
            }
            status = child.wait(), if exited_at.is_none() => {
                exited_at = Some(Instant::now());
                exit_code = status.ok().map(|status| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        status.code().unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal))
                    }
                    #[cfg(not(unix))]
                    { status.code().unwrap_or(1) }
                });
            }
            () = async { tokio::time::sleep_until(grace.unwrap_or_else(|| unreachable!()).into()).await }, if grace.is_some() => {
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
