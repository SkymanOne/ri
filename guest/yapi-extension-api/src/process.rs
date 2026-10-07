//! Processes that run while the extension does other work.

use std::collections::VecDeque;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::task::LocalFuture;
use crate::{op, request};

/// What a running process produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessEvent {
    /// Bytes written to standard output.
    Stdout(Vec<u8>),
    /// Bytes written to standard error.
    Stderr(Vec<u8>),
    /// The process exited, with its code or the name of the signal that
    /// ended it, such as `SIGTERM`. Output written before it exited may
    /// still follow.
    Exit {
        /// The exit code; `None` when a signal ended the process.
        code: Option<i32>,
        /// The signal that ended the process.
        signal: Option<String>,
    },
}

/// A running process. Dropping it kills the process, so a process started
/// by a tool ends when the tool's run is aborted.
pub struct Process {
    id: u64,
    pid: Option<u32>,
    events: VecDeque<ProcessEvent>,
    ended: bool,
    /// The read in progress, kept when a `next` future is dropped so its
    /// output is not lost.
    reading: Option<LocalFuture<Result<Value, String>>>,
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process")
            .field("id", &self.id)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Process {
    /// Starts the process `spawn` describes: `{"command", "args", "cwd",
    /// "env", "stdin"}`, all optional except `command`. Without `cwd` it
    /// starts in yapi's working folder, `env` replaces the whole environment,
    /// and `"stdin": "ignore"` gives it empty standard input instead of a
    /// pipe. Fails with Node's spawn error, such as `spawn git ENOENT`, or
    /// when the extension may not run processes.
    pub fn spawn(spawn: &Value) -> Result<Process, String> {
        let started = request("process.spawn", spawn)?;
        Ok(Process {
            id: started["id"]
                .as_u64()
                .ok_or("process.spawn answered no id")?,
            pid: started["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok()),
            events: VecDeque::new(),
            ended: false,
            reading: None,
        })
    }

    /// The process id.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The next event, waiting for one; `None` once the process has exited
    /// and closed its output. Dropping the future loses no output: the next
    /// call picks up the same read.
    pub async fn next(&mut self) -> Option<ProcessEvent> {
        while self.events.is_empty() && !self.ended {
            let id = self.id;
            let reading = self
                .reading
                .get_or_insert_with(|| Box::pin(op("process.next", &json!({"id": id}))));
            let batch = reading.as_mut().await.unwrap_or_default();
            self.reading = None;
            let batch = batch.as_array().map(Vec::as_slice).unwrap_or_default();
            self.ended = batch.is_empty();
            self.events.extend(batch.iter().map(event));
        }
        self.events.pop_front()
    }

    /// Writes `data` to standard input, waiting while the process has not
    /// taken the previous write; false once it is closed.
    pub async fn write(&self, data: &[u8]) -> bool {
        op(
            "process.write",
            &json!({"id": self.id, "data": STANDARD.encode(data)}),
        )
        .await
        .is_ok_and(|open| open == true)
    }

    /// Closes standard input.
    pub fn close_stdin(&self) {
        let _ = request("process.end", &json!({"id": self.id}));
    }

    /// Sends `signal`, such as `SIGTERM`; false once the process has exited.
    pub fn kill(&self, signal: &str) -> bool {
        request("process.kill", &json!({"id": self.id, "signal": signal}))
            .is_ok_and(|sent| sent == true)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = request("process.release", &json!({"id": self.id}));
    }
}

fn event(event: &Value) -> ProcessEvent {
    let data = || {
        STANDARD
            .decode(event["data"].as_str().unwrap_or_default())
            .unwrap_or_default()
    };
    match event["type"].as_str() {
        Some("stdout") => ProcessEvent::Stdout(data()),
        Some("stderr") => ProcessEvent::Stderr(data()),
        _ => ProcessEvent::Exit {
            code: event["code"]
                .as_i64()
                .and_then(|code| i32::try_from(code).ok()),
            signal: event["signal"].as_str().map(str::to_owned),
        },
    }
}
