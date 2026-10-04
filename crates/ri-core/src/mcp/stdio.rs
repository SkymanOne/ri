//! The stdio transport: a server process speaking newline-delimited JSON-RPC
//! on stdin and stdout. Port of `transports/stdio.ts` in pi-mcp `v1.0.0`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::watch;

use super::jsonrpc::McpError;
use super::transport::{Event, Events, MAX_MESSAGE_BYTES};

const MAX_STDERR_BYTES: usize = 64 * 1024;
/// How long a server may take to exit on its own after stdin closes.
const STDIN_CLOSE_GRACE: Duration = Duration::from_millis(500);
/// How long a server may take to exit after SIGTERM before SIGKILL.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(2000);

/// How to start a stdio server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StdioOptions {
    /// The program.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Its working directory.
    pub cwd: PathBuf,
    /// Variables added to the inherited environment.
    pub env: Vec<(String, String)>,
}

/// A running stdio server.
pub struct StdioTransport {
    options: StdioOptions,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pid: Mutex<Option<u32>>,
    exited: Mutex<Option<watch::Receiver<bool>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    closed: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sends `signal` to the server's process group, so wrappers such as `npx`
/// do not leave the server behind; to the process alone when that fails.
#[cfg(unix)]
fn signal_tree(pid: Option<u32>, signal: rustix::process::Signal) {
    use rustix::process::{Pid, kill_process, kill_process_group};
    let Some(pid) = pid.and_then(|pid| Pid::from_raw(pid as i32)) else {
        return;
    };
    if kill_process_group(pid, signal).is_err() {
        let _ = kill_process(pid, signal);
    }
}

impl StdioTransport {
    /// A transport that starts `options` when the client connects.
    pub fn new(options: StdioOptions) -> StdioTransport {
        StdioTransport {
            options,
            stdin: tokio::sync::Mutex::new(None),
            pid: Mutex::new(None),
            exited: Mutex::new(None),
            stderr: Arc::new(Mutex::new(Vec::new())),
            closed: AtomicBool::new(false),
        }
    }

    /// The end of the server's stderr, at most 64 KiB.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&lock(&self.stderr)).into_owned()
    }

    /// Starts the server; its messages and exit go to `events`.
    pub async fn start(&self, events: Events) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::closed());
        }
        let mut command = tokio::process::Command::new(&self.options.command);
        command
            .args(&self.options.args)
            .current_dir(&self.options.cwd)
            .envs(crate::config::child_env())
            .envs(self.options.env.iter().map(|(key, value)| (key, value)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Its own process group, so closing can stop the server's children too.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                McpError::Other(format!("spawn {} ENOENT", self.options.command))
            } else {
                McpError::Other(error.to_string())
            }
        })?;
        *lock(&self.pid) = child.id();
        *self.stdin.lock().await = child.stdin.take();
        let (exit_tx, exit_rx) = watch::channel(false);
        *lock(&self.exited) = Some(exit_rx.clone());

        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&self.stderr);
            tokio::spawn(async move {
                let mut stderr = stderr;
                let mut buffer = [0u8; 4096];
                while let Ok(count) = stderr.read(&mut buffer).await {
                    if count == 0 {
                        break;
                    }
                    let mut tail = lock(&tail);
                    tail.extend_from_slice(&buffer[..count]);
                    if tail.len() > MAX_STDERR_BYTES {
                        let excess = tail.len() - MAX_STDERR_BYTES;
                        tail.drain(..excess);
                    }
                }
            });
        }
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(read_messages(stdout, events, exit_rx));
        }
        tokio::spawn(async move {
            let _ = child.wait().await;
            let _ = exit_tx.send(true);
        });
        Ok(())
    }

    /// Writes one message as a line.
    pub async fn send(&self, message: &Value) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::closed());
        }
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or_else(McpError::closed)?;
        let line = ri_types::json::to_string(message)
            .map_err(|error| McpError::Other(error.to_string()))?
            + "\n";
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| McpError::closed())?;
        stdin.flush().await.map_err(|_| McpError::closed())
    }

    /// Shuts the server down as the spec asks: stdin closes, then SIGTERM,
    /// then SIGKILL. Children that outlive it are sent SIGTERM.
    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        drop(self.stdin.lock().await.take());
        let Some(mut exited) = lock(&self.exited).clone() else {
            return;
        };
        let pid = *lock(&self.pid);
        let wait = async {
            let _ = exited.wait_for(|done| *done).await;
        };
        tokio::pin!(wait);
        if tokio::time::timeout(STDIN_CLOSE_GRACE, &mut wait)
            .await
            .is_err()
        {
            #[cfg(unix)]
            signal_tree(pid, rustix::process::Signal::TERM);
            if tokio::time::timeout(CLOSE_TIMEOUT, &mut wait)
                .await
                .is_err()
            {
                #[cfg(unix)]
                signal_tree(pid, rustix::process::Signal::KILL);
                wait.await;
            }
        }
        #[cfg(unix)]
        signal_tree(pid, rustix::process::Signal::TERM);
        #[cfg(not(unix))]
        let _ = pid;
    }
}

/// Reads stdout line by line until it ends, then reports the close once the
/// process has exited.
async fn read_messages(
    stdout: tokio::process::ChildStdout,
    events: Events,
    mut exited: watch::Receiver<bool>,
) {
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = Vec::new();
    let mut incomplete = false;
    loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_MESSAGE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !line.ends_with(b"\n") {
            if line.len() > MAX_MESSAGE_BYTES {
                let _ = events.send(Event::Error(format!(
                    "MCP stdio message exceeds {MAX_MESSAGE_BYTES} bytes"
                )));
                // Skip the rest of the oversized line.
                let mut rest = Vec::new();
                let _ = reader.read_until(b'\n', &mut rest).await;
                continue;
            }
            incomplete = !String::from_utf8_lossy(&line).trim().is_empty();
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches('\n').trim_end_matches('\r');
        if text.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<Value>(text) {
            Ok(message) if super::jsonrpc::classify(&message).is_some() => Event::Message(message),
            Ok(_) => Event::Error("Invalid JSON-RPC message".into()),
            Err(error) => Event::Error(error.to_string()),
        };
        let _ = events.send(event);
    }
    let _ = exited.wait_for(|done| *done).await;
    if incomplete {
        let _ = events.send(Event::Error(
            "MCP stdio server closed with an incomplete JSON-RPC message".into(),
        ));
    }
    let _ = events.send(Event::Closed);
}
