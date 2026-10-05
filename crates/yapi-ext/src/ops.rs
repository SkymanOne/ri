//! Host work the guest starts: timers, processes and HTTP requests. Each
//! takes the JSON payload the Node shims send and returns their result shape.

use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// How long a killed process gets between SIGTERM and SIGKILL, as in pi.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// A process to run: `{command, args, cwd, env, input, timeout}`.
struct Spawn {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Map<String, Value>>,
    input: Option<String>,
    timeout: Option<Duration>,
}

impl Spawn {
    fn parse(payload: &Value) -> Spawn {
        let text = |key: &str| payload[key].as_str().map(str::to_owned);
        Spawn {
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

    /// The process to start, with piped output and, when there is input,
    /// piped input. An explicit environment replaces the process's, as in
    /// Node.
    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.command);
        command
            .args(&self.args)
            .stdin(if self.input.is_some() {
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
    let mut command = tokio::process::Command::from(spawn.command());
    command.kill_on_drop(true);
    let mut child = command
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

/// Runs a process and blocks until it exits; the result of [`exec`].
pub(crate) fn exec_sync(payload: &Value) -> Result<Value, String> {
    let spawn = Spawn::parse(payload);
    let mut child = match spawn.command().spawn() {
        Ok(child) => child,
        Err(err) => {
            return Ok(json!({
                "stdout": "", "stderr": "", "code": Value::Null,
                "error": spawn_error(&spawn.command, &err),
            }));
        }
    };
    if let (Some(input), Some(mut stdin)) = (spawn.input, child.stdin.take()) {
        std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let deadline = spawn.timeout.map(|limit| std::time::Instant::now() + limit);
    let mut killed = false;
    if let Some(deadline) = deadline {
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    killed = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
    }
    let output = child.wait_with_output().map_err(|err| err.to_string())?;
    Ok(json!({
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "code": output.status.code(),
        "signal": signal_name(&output.status),
        "killed": killed,
    }))
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

fn signal_name(status: &std::process::ExitStatus) -> Option<&'static str> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().map(|signal| match signal {
            1 => "SIGHUP",
            2 => "SIGINT",
            9 => "SIGKILL",
            15 => "SIGTERM",
            _ => "SIGTERM",
        })
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}
