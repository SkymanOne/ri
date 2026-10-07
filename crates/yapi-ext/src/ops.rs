//! Host work the guest starts: timers, processes and HTTP requests. Each
//! takes the JSON payload the Node shims send and returns their result shape.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use yapi_types::sync::lock;

/// How long a killed process gets between SIGTERM and SIGKILL, as in pi.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Output chunks a running process may have waiting for the guest. A process
/// whose output nobody reads blocks writing, as in Node.
const QUEUED_CHUNKS: usize = 16;

/// A process to run: `{command, args, cwd, env, input, timeout}`.
struct Spawn {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Map<String, Value>>,
    input: Option<String>,
    timeout: Option<Duration>,
    /// Pipe standard input: for `input`, or to write to a running process.
    pipe_stdin: bool,
}

impl Spawn {
    fn parse(payload: &Value) -> Spawn {
        let text = |key: &str| payload[key].as_str().map(str::to_owned);
        Spawn {
            pipe_stdin: payload["input"].is_string(),
            command: text("command").unwrap_or_default(),
            args: payload["args"]
                .as_array()
                .map(|args| {
                    args.iter()
                        .map(|arg| match arg {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            cwd: text("cwd"),
            env: payload["env"].as_object().cloned(),
            input: text("input"),
            timeout: payload["timeout"]
                .as_f64()
                .filter(|ms| *ms > 0.0)
                .map(|ms| Duration::from_millis(ms as u64)),
        }
    }

    /// The process to start, killed when dropped, with piped output and
    /// piped input when asked for. An explicit environment replaces the
    /// process's, as in Node.
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.command);
        command
            .args(&self.args)
            .kill_on_drop(true)
            .stdin(if self.pipe_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(yapi_core::config::child_env());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = self.env_pairs() {
            command.env_clear().envs(env);
        }
        command
    }

    fn env_pairs(&self) -> Option<Vec<(String, String)>> {
        self.env.as_ref().map(|env| {
            env.iter()
                .filter_map(|(key, value)| match value {
                    Value::String(text) => Some((key.clone(), text.clone())),
                    Value::Null => None,
                    other => Some((key.clone(), other.to_string())),
                })
                .collect()
        })
    }
}

/// Waits `{ms}` milliseconds.
pub(crate) async fn timer(payload: Value) -> Result<Value, String> {
    let ms = payload["ms"].as_f64().unwrap_or(0.0).max(0.0);
    tokio::time::sleep(Duration::from_millis(ms as u64)).await;
    Ok(Value::Null)
}

/// Reads `pipe` to its end; empty without a pipe.
async fn read_all(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buffer).await;
    }
    buffer
}

/// Runs a process to completion: `{stdout, stderr, code, signal, killed}`.
/// A process that cannot start fails with Node's spawn error message.
pub(crate) async fn exec(payload: Value) -> Result<Value, String> {
    let spawn = Spawn::parse(&payload);
    let mut child = spawn
        .command()
        .spawn()
        .map_err(|err| spawn_error(&spawn.command, &err))?;
    if let (Some(input), Some(mut stdin)) = (spawn.input.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        });
    }
    let read_out = read_all(child.stdout.take());
    let read_err = read_all(child.stderr.take());
    let mut killed = false;
    let wait = async {
        match spawn.timeout {
            Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    killed = true;
                    terminate(&mut child).await
                }
            },
            None => child.wait().await,
        }
    };
    let (status, out, err) = tokio::join!(wait, read_out, read_err);
    let status = status.map_err(|err| err.to_string())?;
    Ok(json!({
        "stdout": String::from_utf8_lossy(&out),
        "stderr": String::from_utf8_lossy(&err),
        "code": status.code(),
        "signal": signal_name(&status),
        "killed": killed,
    }))
}

/// Runs a process and blocks until it exits: the result of [`exec`], or
/// `{stdout, stderr, code, error}` when it cannot run.
pub(crate) fn exec_sync(payload: &Value) -> Result<Value, String> {
    // Requests run on an instance's thread, outside the tokio runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| err.to_string())?;
    Ok(runtime.block_on(exec(payload.clone())).unwrap_or_else(
        |error| json!({"stdout": "", "stderr": "", "code": Value::Null, "error": error}),
    ))
}

/// The processes one instance runs, by id. Dropping the set or clearing it
/// kills them, so a process never outlives the instance that started it.
#[derive(Clone)]
pub(crate) struct Processes {
    runtime: tokio::runtime::Handle,
    running: Arc<Mutex<HashMap<u64, Running>>>,
    next_id: Arc<AtomicU64>,
}

/// A running process. Its events are out of the map while a `next` call
/// reads them. Dropping it kills the process.
struct Running {
    events: Option<mpsc::Receiver<Value>>,
    /// Closed by `end`.
    stdin: Option<mpsc::UnboundedSender<Vec<u8>>>,
    signals: mpsc::UnboundedSender<Value>,
}

impl Processes {
    /// No processes; theirs run on `runtime`.
    pub(crate) fn new(runtime: tokio::runtime::Handle) -> Processes {
        Processes {
            runtime,
            running: Arc::default(),
            next_id: Arc::default(),
        }
    }

    /// Starts `{command, args, cwd, env, stdin}` and answers `{id, pid}`.
    /// Standard input is piped unless `stdin` is `"ignore"`. A process that
    /// cannot start fails with Node's spawn error message.
    pub(crate) fn spawn(&self, payload: &Value) -> Result<Value, String> {
        let mut spawn = Spawn::parse(payload);
        spawn.pipe_stdin = payload["stdin"] != "ignore";
        // The process registers with the runtime's reaper.
        let _runtime = self.runtime.enter();
        let mut child = spawn
            .command()
            .spawn()
            .map_err(|err| spawn_error(&spawn.command, &err))?;
        let pid = child.id();
        let (sender, events) = mpsc::channel(QUEUED_CHUNKS);
        let stdin = child.stdin.take().map(|mut pipe| {
            let (sender, mut chunks) = mpsc::unbounded_channel::<Vec<u8>>();
            self.runtime.spawn(async move {
                while let Some(chunk) = chunks.recv().await {
                    if pipe.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            });
            sender
        });
        for (name, pipe) in [
            ("stdout", child.stdout.take().map(boxed)),
            ("stderr", child.stderr.take().map(boxed)),
        ] {
            if let Some(pipe) = pipe {
                self.runtime.spawn(forward(name, pipe, sender.clone()));
            }
        }
        let (signals, received) = mpsc::unbounded_channel();
        self.runtime.spawn(watch(child, received, sender));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.running).insert(
            id,
            Running {
                events: Some(events),
                stdin,
                signals,
            },
        );
        Ok(json!({"id": id, "pid": pid}))
    }

    /// The events of process `id` that have arrived, waiting for at least
    /// one: `{type: "stdout" | "stderr", data}` with base64 `data`, and
    /// `{type: "exit", code, signal}`. None once the process has exited and
    /// closed its output.
    pub(crate) async fn next(&self, id: u64) -> Value {
        let Some(mut events) = lock(&self.running)
            .get_mut(&id)
            .and_then(|running| running.events.take())
        else {
            return json!([]);
        };
        let mut batch: Vec<Value> = events.recv().await.into_iter().collect();
        while let Ok(event) = events.try_recv() {
            batch.push(event);
        }
        let mut running = lock(&self.running);
        if batch.is_empty() {
            running.remove(&id);
        } else if let Some(running) = running.get_mut(&id) {
            running.events = Some(events);
        }
        Value::Array(batch)
    }

    /// Writes base64 `{id, data}` to process `id`'s standard input; whether
    /// it is still open.
    pub(crate) fn write(&self, payload: &Value) -> Result<Value, String> {
        let data = STANDARD
            .decode(payload["data"].as_str().unwrap_or_default())
            .map_err(|err| err.to_string())?;
        Ok(json!(self.with(payload, |running| {
            running
                .stdin
                .as_ref()
                .is_some_and(|stdin| stdin.send(data).is_ok())
        })))
    }

    /// Closes process `{id}`'s standard input.
    pub(crate) fn end(&self, payload: &Value) {
        self.with(payload, |running| running.stdin = None);
    }

    /// Sends `{id, signal}`, a signal name or number, to process `id`;
    /// whether it was still running.
    pub(crate) fn kill(&self, payload: &Value) -> Value {
        let signal = payload["signal"].clone();
        json!(self.with(payload, |running| running.signals.send(signal).is_ok()))
    }

    /// Kills every process.
    pub(crate) fn clear(&self) {
        lock(&self.running).clear();
    }

    /// Runs `f` on process `{id}`; false when there is none.
    fn with<T: Default>(&self, payload: &Value, f: impl FnOnce(&mut Running) -> T) -> T {
        let id = payload["id"].as_u64().unwrap_or(u64::MAX);
        lock(&self.running).get_mut(&id).map(f).unwrap_or_default()
    }
}

fn boxed(
    pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) -> Box<dyn tokio::io::AsyncRead + Unpin + Send> {
    Box::new(pipe)
}

/// Sends what `pipe` produces as `{type: name, data}` events until it ends.
async fn forward(
    name: &'static str,
    mut pipe: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    events: mpsc::Sender<Value>,
) {
    let mut buffer = vec![0; 64 * 1024];
    while let Ok(read @ 1..) = pipe.read(&mut buffer).await {
        let event = json!({"type": name, "data": STANDARD.encode(&buffer[..read])});
        if events.send(event).await.is_err() {
            break;
        }
    }
}

/// Waits for `child` to exit and reports it, delivering the signals sent to
/// it meanwhile. Once nobody can signal it, it is killed.
async fn watch(
    mut child: tokio::process::Child,
    mut signals: mpsc::UnboundedReceiver<Value>,
    events: mpsc::Sender<Value>,
) {
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            signal = signals.recv() => match signal {
                Some(signal) => send_signal(&mut child, &signal),
                None => {
                    let _ = child.start_kill();
                    break child.wait().await;
                }
            },
        }
    };
    if let Ok(status) = status {
        let _ = events
            .send(json!({"type": "exit", "code": status.code(), "signal": signal_name(&status)}))
            .await;
    }
}

/// Sends `signal`, a name such as `SIGTERM` or a number, to `child`. Other
/// platforms only kill.
fn send_signal(child: &mut tokio::process::Child, signal: &Value) {
    #[cfg(unix)]
    {
        let known = SIGNALS.iter().find(|(known, name)| match signal {
            Value::Number(number) => number.as_i64() == Some(i64::from(known.as_raw())),
            _ => signal.as_str().unwrap_or("SIGTERM") == *name,
        });
        let pid = child
            .id()
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32));
        if let (Some((known, _)), Some(pid)) = (known, pid) {
            let _ = rustix::process::kill_process(pid, *known);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
        let _ = child.start_kill();
    }
}

/// Sends `{url, method, headers, body | bodyBase64}`; answers `{status,
/// statusText, headers, bodyBase64}`.
pub(crate) async fn fetch(payload: Value) -> Result<Value, String> {
    let url = payload["url"].as_str().unwrap_or_default();
    let method = payload["method"].as_str().unwrap_or("GET");
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|err| err.to_string())?;
    let mut request = yapi_ai::http::client().request(method, url);
    if let Some(headers) = payload["headers"].as_object() {
        for (name, value) in headers {
            if let Some(value) = value.as_str() {
                request = request.header(name, value);
            }
        }
    }
    if let Some(body) = payload["body"].as_str() {
        request = request.body(body.to_owned());
    } else if let Some(body) = payload["bodyBase64"].as_str() {
        request = request.body(STANDARD.decode(body).map_err(|err| err.to_string())?);
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("fetch failed: {err}"))?;
    let status = response.status();
    let mut headers = Map::new();
    for (name, value) in response.headers() {
        if let Ok(value) = value.to_str() {
            headers.insert(name.as_str().to_owned(), Value::String(value.to_owned()));
        }
    }
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("fetch failed: {err}"))?;
    Ok(json!({
        "status": status.as_u16(),
        "statusText": status.canonical_reason().unwrap_or_default(),
        "headers": headers,
        "bodyBase64": STANDARD.encode(&body),
    }))
}

/// Resolves `{hostname}` with the system resolver, as Node's `dns.lookup`
/// does; answers `{addresses: [{address, family}]}` in the resolver's order,
/// or `{error}` when the name does not resolve.
pub(crate) async fn dns_lookup(payload: Value) -> Result<Value, String> {
    let hostname = payload["hostname"].as_str().unwrap_or_default().to_owned();
    let resolved = tokio::task::spawn_blocking(move || {
        std::net::ToSocketAddrs::to_socket_addrs(&(hostname.as_str(), 0))
            .map(|addresses| addresses.map(|address| address.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|err| err.to_string())?;
    let ips = match resolved {
        Ok(ips) => ips,
        Err(err) => return Ok(json!({ "error": err.to_string() })),
    };
    let mut addresses: Vec<Value> = Vec::new();
    for ip in ips {
        let entry = json!({
            "address": ip.to_string(),
            "family": if ip.is_ipv4() { 4 } else { 6 },
        });
        if !addresses.contains(&entry) {
            addresses.push(entry);
        }
    }
    Ok(json!({ "addresses": addresses }))
}

async fn terminate(child: &mut tokio::process::Child) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid as i32).ok_or(std::io::ErrorKind::InvalidInput)?,
            rustix::process::Signal::TERM,
        );
        if let Ok(status) = tokio::time::timeout(KILL_GRACE, child.wait()).await {
            return status;
        }
    }
    child.kill().await?;
    child.wait().await
}

fn spawn_error(command: &str, err: &std::io::Error) -> String {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        _ => "EIO",
    };
    format!("spawn {command} {code}")
}

/// The signals Node names, as its `signo_string` does; others have no name.
#[cfg(unix)]
const SIGNALS: &[(rustix::process::Signal, &str)] = {
    use rustix::process::Signal;
    &[
        (Signal::HUP, "SIGHUP"),
        (Signal::INT, "SIGINT"),
        (Signal::QUIT, "SIGQUIT"),
        (Signal::ILL, "SIGILL"),
        (Signal::TRAP, "SIGTRAP"),
        (Signal::ABORT, "SIGABRT"),
        (Signal::BUS, "SIGBUS"),
        (Signal::FPE, "SIGFPE"),
        (Signal::KILL, "SIGKILL"),
        (Signal::USR1, "SIGUSR1"),
        (Signal::SEGV, "SIGSEGV"),
        (Signal::USR2, "SIGUSR2"),
        (Signal::PIPE, "SIGPIPE"),
        (Signal::ALARM, "SIGALRM"),
        (Signal::TERM, "SIGTERM"),
        (Signal::CHILD, "SIGCHLD"),
        #[cfg(target_os = "linux")]
        (Signal::STKFLT, "SIGSTKFLT"),
        (Signal::CONT, "SIGCONT"),
        (Signal::STOP, "SIGSTOP"),
        (Signal::TSTP, "SIGTSTP"),
        (Signal::TTIN, "SIGTTIN"),
        (Signal::TTOU, "SIGTTOU"),
        (Signal::URG, "SIGURG"),
        (Signal::XCPU, "SIGXCPU"),
        (Signal::XFSZ, "SIGXFSZ"),
        (Signal::VTALARM, "SIGVTALRM"),
        (Signal::PROF, "SIGPROF"),
        (Signal::WINCH, "SIGWINCH"),
        (Signal::IO, "SIGIO"),
        #[cfg(target_os = "linux")]
        (Signal::POWER, "SIGPWR"),
        #[cfg(target_os = "macos")]
        (Signal::INFO, "SIGINFO"),
        (Signal::SYS, "SIGSYS"),
    ]
};

/// The name of the signal that ended a process, as Node reports it.
fn signal_name(status: &std::process::ExitStatus) -> Option<&'static str> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        let signal = status.signal()?;
        SIGNALS
            .iter()
            .find(|(known, _)| known.as_raw() == signal)
            .map(|(_, name)| *name)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test helpers; a panic is a test failure"
    )]

    use super::*;

    #[test]
    fn names_signals_as_node_does() {
        for name in ["SEGV", "USR1", "TERM"] {
            let script = format!("kill -s {name} $$");
            let result = exec_sync(&json!({"command": "/bin/sh", "args": ["-c", script]})).unwrap();
            assert_eq!(result["signal"], format!("SIG{name}"), "{result}");
            assert_eq!(result["code"], Value::Null, "{result}");
        }
    }

    /// Output beyond a pipe's buffer must not stall a process until its
    /// timeout kills it; a timeout stops it with SIGTERM, as in Node.
    #[test]
    fn exec_sync_reads_output_while_it_waits() {
        let run = |script: &str, timeout: u64| {
            exec_sync(&json!({"command": "/bin/sh", "args": ["-c", script], "timeout": timeout}))
                .unwrap()
        };
        let result = run("head -c 200000 /dev/zero", 5000);
        assert_eq!(result["killed"], false);
        assert_eq!(result["stdout"].as_str().map(str::len), Some(200_000));
        let result = run("sleep 10", 100);
        assert_eq!(result["killed"], true);
        assert_eq!(result["signal"], "SIGTERM");
    }

    /// Every event of process `id` until it ends, with output decoded.
    async fn events(processes: &Processes, id: u64) -> (String, Vec<Value>) {
        let mut output = String::new();
        let mut exits = Vec::new();
        loop {
            let batch = processes.next(id).await;
            let Value::Array(batch) = batch else {
                panic!("{batch}")
            };
            if batch.is_empty() {
                return (output, exits);
            }
            for event in batch {
                match event["type"].as_str() {
                    Some("exit") => exits.push(event),
                    _ => output.push_str(
                        &String::from_utf8(
                            STANDARD.decode(event["data"].as_str().unwrap()).unwrap(),
                        )
                        .unwrap(),
                    ),
                }
            }
        }
    }

    fn spawn(processes: &Processes, payload: Value) -> u64 {
        processes.spawn(&payload).unwrap()["id"].as_u64().unwrap()
    }

    /// Input reaches a running process and its output streams back.
    #[tokio::test(flavor = "multi_thread")]
    async fn processes_take_input_and_stream_output() {
        let processes = Processes::new(tokio::runtime::Handle::current());
        let id = spawn(&processes, json!({"command": "/bin/cat"}));
        let write = |text: &str| processes.write(&json!({"id": id, "data": STANDARD.encode(text)}));
        assert_eq!(write("one ").unwrap(), true);
        assert_eq!(write("two").unwrap(), true);
        processes.end(&json!({"id": id}));
        let (output, exits) = events(&processes, id).await;
        assert_eq!(output, "one two");
        assert_eq!(exits, [json!({"type": "exit", "code": 0, "signal": null})]);
        // Without piped input the process reads nothing.
        let id = spawn(
            &processes,
            json!({"command": "/bin/cat", "stdin": "ignore"}),
        );
        assert_eq!(write("lost").unwrap(), false);
        assert_eq!(events(&processes, id).await.0, "");
        let error = processes
            .spawn(&json!({"command": "/no/such/program"}))
            .unwrap_err();
        assert_eq!(error, "spawn /no/such/program ENOENT");
    }

    /// `kill` signals a running process by name or number and reports
    /// whether it was still running.
    #[tokio::test(flavor = "multi_thread")]
    async fn processes_take_signals() {
        let processes = Processes::new(tokio::runtime::Handle::current());
        for signal in [json!("SIGINT"), json!(15), Value::Null] {
            let id = spawn(&processes, json!({"command": "/bin/sleep", "args": ["10"]}));
            assert_eq!(processes.kill(&json!({"id": id, "signal": signal})), true);
            let (_, exits) = events(&processes, id).await;
            let name = if signal == "SIGINT" {
                "SIGINT"
            } else {
                "SIGTERM"
            };
            assert_eq!(exits[0]["signal"], name);
            assert_eq!(processes.kill(&json!({"id": id})), false);
        }
    }

    /// Clearing the set kills its processes.
    #[tokio::test(flavor = "multi_thread")]
    async fn cleared_processes_are_killed() {
        let processes = Processes::new(tokio::runtime::Handle::current());
        let reply = processes
            .spawn(&json!({"command": "/bin/sleep", "args": ["10"]}))
            .unwrap();
        let pid = rustix::process::Pid::from_raw(reply["pid"].as_i64().unwrap() as i32).unwrap();
        processes.clear();
        for _ in 0..100 {
            if rustix::process::test_kill_process(pid).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the process outlived the set");
    }
}
