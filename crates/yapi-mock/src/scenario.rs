//! End-to-end scenarios: run pi or yapi as a black box against a cassette, and
//! normalize what they print and send so the two can be compared.
//!
//! A scenario is a cassette, command-line arguments and files to create in a fresh
//! working directory. The agent directory gets a `models.json` pointing the
//! providers at the mock server; credentials are dummy environment variables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{Cassette, Error, MockServer, RecordedRequest};

/// One scenario from `tests/fixtures/scenarios/scenarios.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    /// Name; also the golden file name.
    pub name: String,
    /// Cassette path relative to `tests/fixtures/cassettes`.
    pub cassette: String,
    /// Arguments after the program name.
    pub args: Vec<String>,
    /// Files to create in the working directory, by relative path.
    #[serde(default)]
    pub files: IndexMap<String, String>,
    /// Text on stdin; stdin is empty otherwise.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Files to create in the agent directory, such as `settings.json`.
    #[serde(default, rename = "agentFiles")]
    pub agent_files: IndexMap<String, String>,
    /// Runs the program in a terminal and types into it.
    #[serde(default)]
    pub tty: Option<Tty>,
    /// Commands for RPC mode, sent one at a time; stdin closes after the last.
    #[serde(default)]
    pub rpc: Option<Vec<RpcStep>>,
    /// A Node script in the fixture generator that runs instead, with the
    /// program as its first argument, followed by `args`. It needs the
    /// generator's `node_modules`.
    #[serde(default)]
    pub client: Option<String>,
    /// The operating system the golden was recorded on, for screens that
    /// show text pi chooses by platform, such as macOS's Option key name.
    /// Such a scenario runs only there.
    #[serde(default)]
    pub os: Option<String>,
}

impl Scenario {
    /// Why this machine cannot run the scenario, if it cannot: client
    /// scripts need Node and the fixture generator's packages, and a golden
    /// recorded for one operating system holds only there.
    pub fn skip_reason(&self) -> Option<&'static str> {
        if self.client.is_some() && !generator_dir().join("node_modules").is_dir() {
            return Some("needs the fixture generator's packages");
        }
        if self
            .os
            .as_deref()
            .is_some_and(|os| os != std::env::consts::OS)
        {
            return Some("its golden shows another operating system's text");
        }
        None
    }
}

/// The pi fixture generator, with pi installed in its `node_modules`.
pub fn generator_dir() -> PathBuf {
    fixtures_dir().join("pi").join("generator")
}

/// One command of an RPC scenario.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcStep {
    /// The command, written as one JSON line; a string is written verbatim
    /// and `null` writes nothing. A string value `{{<id>:<pointer>}}` inside
    /// the command becomes the value at that JSON pointer of the response to
    /// command `<id>`.
    pub send: Value,
    /// Waits for an output line of this `type` before the next step. Without
    /// it, waits for the response to the command's `id`, if it has one.
    #[serde(default)]
    pub until: Option<String>,
}

/// The longest wait for one RPC step.
const RPC_STEP_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A terminal session: its size and what to type. After starting and after
/// each key the screen is left to settle, unless the next step waits for text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tty {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Steps: input to send, one write each, such as `"hello"` or `"\r"`, or
    /// text to wait for.
    pub keys: Vec<Key>,
    /// Whether to record the terminal progress sequences (OSC 9;4) written.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub progress: bool,
}

/// One step of a terminal session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Key {
    /// Input to send.
    Input(String),
    /// Text to wait for on screen instead of waiting for the screen to settle,
    /// for screens that keep changing, such as a dialog's countdown.
    Wait {
        /// The text.
        wait: String,
    },
}

/// Which program runs a scenario.
#[derive(Clone, Debug)]
pub enum Program {
    /// pi, at this executable.
    Pi(PathBuf),
    /// yapi, at this executable.
    Yapi(PathBuf),
}

/// What a run produced, before normalization.
#[derive(Clone, Debug)]
pub struct Run {
    /// Process exit code.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Requests the mock received.
    pub requests: Vec<RecordedRequest>,
    /// The working directory, for normalization.
    pub cwd: PathBuf,
    /// The agent directory, for normalization.
    pub agent_dir: PathBuf,
    /// Session files in the working and agent directories after the run.
    pub sessions: Vec<SessionFile>,
    /// The final screen of a terminal run, one string per row.
    pub screen: Option<Vec<String>>,
    /// The progress sequences of a terminal run that records them.
    pub progress: Option<Vec<String>>,
    /// The mock server's URL, for normalization.
    pub url: String,
}

/// A session file found after a run.
#[derive(Clone, Debug)]
pub struct SessionFile {
    /// File name.
    pub name: String,
    /// Whether the scenario created it before the run.
    pub seeded: bool,
    /// Contents.
    pub content: String,
}

fn session_files(dir: &Path, found: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            session_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            found.push((path, content));
        }
    }
}

/// The repository's `tests/fixtures` directory.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// Loads every scenario.
pub fn load_scenarios() -> Result<Vec<Scenario>, Error> {
    crate::cassette::load_json(&fixtures_dir().join("scenarios/scenarios.json"))
}

/// Compiles a pattern of this file; each is a valid literal.
fn pattern(text: &str) -> Regex {
    Regex::new(text).expect("the normalizer's patterns are valid")
}

/// pi's sign-in help: a providers.md line followed by a models.md path.
static PROVIDERS_DOC: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?m)^[^\n]*/pi-coding-agent/docs/providers\.md\n[^\n]*/pi-coding-agent/docs/models\.md",
    )
});
/// pi's `<docs>` section of the system prompt.
static DOCS_SECTION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\n\n<docs>\n(?s:(?:.*?\n)?)</docs>"));
/// A path to pi's codemode reference.
static CODEMODE_DOC: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"[^\s"]*/pi-coding-agent/docs/codemode\.md"#));
/// Codemode's wall time line.
static WALL_TIME: LazyLock<Regex> = LazyLock::new(|| pattern(r"\nWall time [0-9.]* seconds\n"));
/// A new session file name: a timestamp and the session id.
static SESSION_FILE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?s)(.{23}Z_([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}))\.jsonl",
    )
});
/// A UUID such as pi and yapi use for session ids.
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
});
/// A codemode call row ending in its duration.
static CALL_DURATION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(\s*[✓✗⊘…].*) (?:[0-9]+ms|[0-9]+\.[0-9]s)$"));

/// The length of every run's root directory path. Screens show paths under the
/// root, and pi's goldens wrap them where a root of 36 to 54 characters does:
/// some rows need a path to fit, others need it to move to the next row. A
/// fixed length in the middle keeps the process id, the run counter and the
/// platform's temporary directory, such as macOS's `/private/tmp`, from moving
/// those wraps.
const ROOT_LEN: usize = 45;

/// A fresh directory for one run, named after the scenario and padded or
/// truncated to [`ROOT_LEN`].
fn scratch_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = scratch_base();
    let unique = format!("yapi-{}-{n}", std::process::id());
    let len = ROOT_LEN
        .saturating_sub(base.to_string_lossy().chars().count() + 1)
        .max(unique.len());
    let dir: String = format!("{unique}-{name}")
        .chars()
        .chain(std::iter::repeat('-'))
        .take(len)
        .collect();
    base.join(dir)
}

/// Where scenarios run: `/tmp` with symlinks resolved, as programs report
/// their working directory. macOS's default temporary directory sits behind
/// the `/var` symlink, and its length would wrap screens differently from
/// the goldens.
fn scratch_base() -> PathBuf {
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    base.canonicalize().unwrap_or(base)
}

/// Runs a scenario: starts the mock server, prepares the directories, runs the
/// program to completion and collects its output and the requests.
pub async fn run(scenario: &Scenario, program: &Program) -> Result<Run, Error> {
    let cassette = Cassette::load(&fixtures_dir().join("cassettes").join(&scenario.cassette))?;
    let server =
        MockServer::start(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), cassette).await?;
    let url = server.url();
    let root = scratch_dir(&scenario.name);
    let cwd = root.join("project");
    let agent_dir = root.join("agent");
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Read { path, source }
    };
    std::fs::create_dir_all(&cwd).map_err(io(&cwd))?;
    std::fs::create_dir_all(&agent_dir).map_err(io(&agent_dir))?;
    // Project configuration is `.yapi/` for yapi and `.pi/` for pi.
    let project_file = |relative: &str| match (program, relative.strip_prefix(".yapi/")) {
        (Program::Pi(_), Some(rest)) => cwd.join(".pi").join(rest),
        _ => cwd.join(relative),
    };
    let files = scenario
        .files
        .iter()
        .map(|(relative, content)| (project_file(relative), content))
        .chain(
            scenario
                .agent_files
                .iter()
                .map(|(relative, content)| (agent_dir.join(relative), content)),
        );
    let mut seeded = Vec::new();
    for (path, content) in files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        let content = content
            .replace("{{cwd}}", &cwd.to_string_lossy())
            .replace("{{agent}}", &agent_dir.to_string_lossy())
            .replace("{{fixtures}}", &fixtures_dir().to_string_lossy());
        std::fs::write(&path, content).map_err(io(&path))?;
        seeded.push(path);
    }
    let mut models = json!({"providers": {
        "anthropic": {"baseUrl": url},
        "groq": {"baseUrl": url},
        "openai": {"baseUrl": format!("{url}/v1")},
        "google": {"baseUrl": format!("{url}/v1beta")},
        "openai-codex": {"baseUrl": url},
        "azure-openai-responses": {"baseUrl": format!("{url}/openai/v1")},
        "mistral": {"baseUrl": url},
        "opencode-go": {"baseUrl": url},
    }});
    let models_path = agent_dir.join("models.json");
    // A scenario's own `models.json` keeps its settings; providers it leaves
    // without a base URL still point at the mock.
    if let Some(Value::Object(seeded)) = std::fs::read_to_string(&models_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|mut value| value.get_mut("providers").map(Value::take))
        && let Some(Value::Object(defaults)) = models.get_mut("providers")
    {
        for (provider, config) in seeded {
            let entry = defaults.entry(provider).or_insert_with(|| json!({}));
            if let (Value::Object(entry), Value::Object(config)) = (entry, config) {
                for (key, value) in config {
                    entry.insert(key, value);
                }
            }
        }
    }
    std::fs::write(&models_path, models.to_string()).map_err(io(&models_path))?;

    let (executable, dir_var) = match program {
        Program::Pi(path) => (path.clone(), "PI_CODING_AGENT_DIR"),
        Program::Yapi(path) => (path.clone(), "YAPI_CODING_AGENT_DIR"),
    };
    let (executable, args) = match &scenario.client {
        Some(client) => {
            let mut args = vec![
                generator_dir().join(client).display().to_string(),
                executable.display().to_string(),
            ];
            args.extend(scenario.args.iter().cloned());
            (PathBuf::from("node"), args)
        }
        None => (executable, scenario.args.clone()),
    };
    let executable = &executable;
    // A clean environment: ambient credentials on the host must not change what
    // either program sees.
    let env: Vec<(&'static str, std::ffi::OsString)> = vec![
        ("PATH", std::env::var_os("PATH").unwrap_or_default()),
        (dir_var, agent_dir.clone().into_os_string()),
        ("HOME", root.clone().into_os_string()),
        ("PI_OFFLINE", "1".into()),
        ("PI_SKIP_VERSION_CHECK", "1".into()),
        ("ANTHROPIC_API_KEY", "mock".into()),
        ("GROQ_API_KEY", "mock".into()),
        ("OPENAI_API_KEY", "mock".into()),
        ("GEMINI_API_KEY", "mock".into()),
        ("OPENCODE_API_KEY", "mock".into()),
    ];
    let mut progress = None;
    let (exit_code, stdout, stderr, screen) = match &scenario.tty {
        Some(tty) => {
            let (path, args, dir, tty) = (executable.clone(), args, cwd.clone(), tty.clone());
            let (exit_code, screen, written) =
                tokio::task::spawn_blocking(move || run_tty(&path, &args, &dir, &env, &tty))
                    .await
                    .map_err(|error| std::io::Error::other(error.to_string()))
                    .and_then(|result| result)
                    .map_err(io(executable))?;
            progress = written;
            (exit_code, String::new(), String::new(), Some(screen))
        }
        None if scenario.rpc.is_some() => {
            let steps = scenario.rpc.as_deref().unwrap_or_default();
            let (exit_code, stdout, stderr) = run_rpc(executable, &args, &cwd, &env, steps)
                .await
                .map_err(io(executable))?;
            (exit_code, stdout, stderr, None)
        }
        None => {
            let mut command = tokio::process::Command::new(executable);
            command
                .args(&args)
                .current_dir(&cwd)
                .env_clear()
                .envs(env.iter().map(|(key, value)| (key, value)));
            command
                .stdin(if scenario.stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().map_err(io(executable))?;
            if let (Some(text), Some(mut stdin)) = (&scenario.stdin, child.stdin.take()) {
                use tokio::io::AsyncWriteExt;
                stdin
                    .write_all(text.as_bytes())
                    .await
                    .map_err(io(executable))?;
            }
            let output = child.wait_with_output().await.map_err(io(executable))?;
            (
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
                None,
            )
        }
    };
    let requests = server.requests();
    drop(server);
    let mut found = Vec::new();
    session_files(&cwd, &mut found);
    session_files(&agent_dir, &mut found);
    let sessions = found
        .into_iter()
        .map(|(path, content)| SessionFile {
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            seeded: seeded.contains(&path),
            content,
        })
        .collect();
    let run = Run {
        exit_code,
        stdout,
        stderr,
        screen,
        progress,
        url,
        requests,
        cwd: cwd.clone(),
        agent_dir,
        sessions,
    };
    let _ = std::fs::remove_dir_all(&root);
    Ok(run)
}

/// Replaces `{{<id>:<pointer>}}` strings in `value` from `responses`.
fn substitute(value: &Value, responses: &HashMap<String, Value>) -> Value {
    match value {
        Value::String(text) => text
            .strip_prefix("{{")
            .and_then(|rest| rest.strip_suffix("}}"))
            .and_then(|reference| reference.split_once(':'))
            .and_then(|(id, pointer)| responses.get(id)?.pointer(pointer).cloned())
            .unwrap_or_else(|| value.clone()),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute(item, responses))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, item)| (key.clone(), substitute(item, responses)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Runs `executable` with piped stdio, writing each step's command once the
/// previous step's awaited line has appeared, then closing stdin. Returns the
/// exit code, stdout and stderr.
async fn run_rpc(
    executable: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(&'static str, std::ffi::OsString)],
    steps: &[RpcStep],
) -> std::io::Result<(i32, String, String)> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("no stderr"))?;
    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    let reader = tokio::spawn(async move {
        let mut all = String::new();
        let mut stdout = tokio::io::BufReader::new(stdout).lines();
        while let Ok(Some(line)) = stdout.next_line().await {
            all.push_str(&line);
            all.push('\n');
            let _ = lines_tx.send(line);
        }
        all
    });
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let mut responses: HashMap<String, Value> = HashMap::new();
    for step in steps {
        let send = substitute(&step.send, &responses);
        let line = match &send {
            Value::String(text) => text.clone(),
            command => command.to_string(),
        };
        if !send.is_null() {
            stdin.write_all(format!("{line}\n").as_bytes()).await?;
            stdin.flush().await?;
        }
        let id = send.get("id").cloned();
        let awaited = |line: &str| {
            let Ok(output) = serde_json::from_str::<Value>(line) else {
                return false;
            };
            match (&step.until, &id) {
                (Some(kind), _) => output["type"] == *kind.as_str(),
                (None, Some(id)) => output["type"] == "response" && output["id"] == *id,
                (None, None) => true,
            }
        };
        if step.until.is_none() && id.is_none() {
            continue;
        }
        let wait = async {
            while let Some(line) = lines.recv().await {
                if let Ok(output) = serde_json::from_str::<Value>(&line)
                    && output["type"] == "response"
                    && let Some(id) = output["id"].as_str()
                {
                    responses.insert(id.to_owned(), output.clone());
                }
                if awaited(&line) {
                    return true;
                }
            }
            false
        };
        if !tokio::time::timeout(RPC_STEP_LIMIT, wait)
            .await
            .unwrap_or(false)
        {
            return Err(std::io::Error::other(format!(
                "RPC step {line} got no awaited output"
            )));
        }
    }
    drop(stdin);
    let status = tokio::time::timeout(RPC_STEP_LIMIT, child.wait())
        .await
        .map_err(|_| std::io::Error::other("RPC process did not exit after stdin closed"))??;
    let stdout = reader.await.map_err(std::io::Error::other)?;
    let stderr = errors.await.map_err(std::io::Error::other)?;
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

/// Runs `executable` in a pseudo-terminal, typing `tty.keys` once the screen
/// settles each time. Returns the exit code (-1 if it was still running), the
/// final screen rows and, when `tty.progress` asks, the progress sequences.
fn run_tty(
    executable: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(&'static str, std::ffi::OsString)],
    tty: &Tty,
) -> std::io::Result<(i32, Vec<String>, Option<Vec<String>>)> {
    let mut pty = crate::pty::Pty::spawn(executable, args, cwd, env, (tty.cols, tty.rows), false)?;
    pty.settle();
    for (index, key) in tty.keys.iter().enumerate() {
        match key {
            Key::Input(input) => {
                pty.write(input)?;
                if !matches!(tty.keys.get(index + 1), Some(Key::Wait { .. })) {
                    pty.settle();
                }
            }
            // A missing text shows in the comparison of the final screen.
            Key::Wait { wait } => {
                let _ = pty.wait_for(crate::pty::SETTLE_LIMIT, |rows| {
                    rows.iter().any(|row| row.contains(wait.as_str()))
                });
            }
        }
    }
    let screen = pty.rows();
    let progress = tty.progress.then(|| pty.progress());
    Ok((pty.finish()?, screen, progress))
}

/// Normalizes runs for comparison: paths, names, timestamps, and ids, which
/// become sequential placeholders so references between them still compare.
struct Normalizer<'a> {
    run: &'a Run,
    ids: HashMap<String, String>,
    renames: Vec<(String, String)>,
}

const ID_KEYS: &[&str] = &[
    "id",
    "sessionId",
    "leafId",
    "entryId",
    "parentId",
    "targetId",
    "fromId",
    "firstKeptEntryId",
    "responseId",
    "toolCallId",
    "prompt_cache_key",
];

impl Normalizer<'_> {
    /// Text normalization: paths, new session file names, yapi's name in the
    /// prompt, pi's docs section.
    fn text(&mut self, text: &str) -> String {
        let mut text = text
            .replace(&fixtures_dir().to_string_lossy().into_owned(), "<fixtures>")
            .replace(
                &self.run.agent_dir.to_string_lossy().into_owned(),
                "<agent>",
            )
            .replace(&self.run.cwd.to_string_lossy().into_owned(), "<cwd>")
            .replace(
                &self
                    .run
                    .cwd
                    .parent()
                    .map(|root| root.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "<root>",
            )
            .replace(&self.run.url, "<mock>")
            .replace(&encoded_dir(&self.run.cwd), "<cwd-dir>")
            .replace("operating inside yapi,", "operating inside pi,")
            .replace(
                "extensions using \"yapi -ne\"",
                "extensions using \"pi -ne\"",
            )
            .replace(
                "allows yapi to load .yapi settings",
                "allows pi to load .pi settings",
            )
            .replace("Restart yapi for this", "Restart pi for this")
            .replace("Project .yapi resources", "Project .pi resources")
            .replace("then restart yapi.", "then restart pi.")
            // Command names in usage text.
            .replace("yapi auth ", "pi auth ")
            .replace("\"yapi auth\"", "\"pi auth\"")
            .replace("\"yapi --help\"", "\"pi --help\"")
            .replace("Usage: yapi ", "Usage: pi ")
            .replace("yapi mcp ", "pi mcp ")
            .replace("yapi config ", "pi config ")
            .replace("this session: yapi --session", "this session: pi --session")
            .replace(".yapi/mcp.json", ".pi/mcp.json")
            .replace(".yapi/settings.json", ".pi/settings.json")
            .replace("~/.yapi/agent", "~/.pi/agent")
            .replace("start yapi in the project", "start pi in the project")
            // The sign-in help points at yapi's README; pi's into its install.
            .replace(
                "  https://github.com/SkymanOne/ri#models-and-sign-in",
                "  <sign-in help>",
            );
        text = PROVIDERS_DOC
            .replace_all(&text, "  <sign-in help>")
            .into_owned();
        for (from, to) in &self.renames {
            text = text.replace(from, to);
        }
        // pi's documentation section points into pi's install; yapi has none.
        let text = DOCS_SECTION.replace_all(&text, "");
        // Codemode's reference: in pi's install, and in yapi's agent directory.
        let text = text.replace("<agent>/docs/codemode.md", "<codemode docs>");
        let text = CODEMODE_DOC.replace_all(&text, "<codemode docs>");
        // Codemode results report the script's wall time.
        let text = WALL_TIME.replace_all(&text, "\nWall time <seconds> seconds\n");
        self.unsaved_session_files(&text)
    }

    /// Names of new session files that were never written, such as an empty
    /// fork's, as `<session <id>>.jsonl` with the id normalized.
    fn unsaved_session_files(&mut self, text: &str) -> String {
        SESSION_FILE
            .replace_all(text, |captures: &regex_lite::Captures| {
                let name = &captures[1];
                let seeded = self
                    .run
                    .sessions
                    .iter()
                    .any(|file| file.seeded && file.name.strip_suffix(".jsonl") == Some(name));
                if seeded {
                    return captures[0].to_owned();
                }
                let id = self.id(&Value::from(&captures[2]));
                format!("<session {}>.jsonl", id.as_str().unwrap_or_default())
            })
            .into_owned()
    }

    fn id(&mut self, value: &Value) -> Value {
        let raw = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let next = self.ids.len() + 1;
        Value::from(
            self.ids
                .entry(raw)
                .or_insert_with(|| format!("<id-{next}>"))
                .clone(),
        )
    }

    fn value(&mut self, value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut out = Map::new();
                for (key, item) in object {
                    let normalized = match (key.as_str(), item) {
                        (key, Value::String(_) | Value::Number(_)) if ID_KEYS.contains(&key) => {
                            self.id(item)
                        }
                        (
                            "timestamp"
                            | "estimatedTokensAfter"
                            | "durationMs"
                            | "wall_time_seconds",
                            Value::String(_) | Value::Number(_),
                        ) => Value::from(format!("<{key}>")),
                        ("sections", Value::Object(sections)) => {
                            let mut kept = Map::new();
                            for (name, text) in sections {
                                if name != "docs" {
                                    kept.insert(name.clone(), self.value(text));
                                }
                            }
                            Value::Object(kept)
                        }
                        _ => self.value(item),
                    };
                    out.insert(key.clone(), normalized);
                }
                Value::Object(out)
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.value(item)).collect())
            }
            Value::String(text) => Value::String(self.text(text)),
            other => other.clone(),
        }
    }
}

/// The comparable form of a run: exit code, stdout as events (JSON mode) or
/// text, the requests' method, path, query and body, and session files.
///
/// pi serializes live objects, so `message_update.usage` and the partial in an
/// assistant `message_start` depend on network timing; both are dropped.
/// `estimatedTokensAfter` estimates the whole prompt, which includes pi's
/// `docs` section, so it is masked too. Parallel tools finish in any order, so
/// each run of consecutive tool update and end events is sorted.
pub fn normalize(run: &Run) -> Value {
    let mut new_files: Vec<&SessionFile> =
        run.sessions.iter().filter(|file| !file.seeded).collect();
    new_files.sort_by(|a, b| a.name.cmp(&b.name));
    let renames = new_files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.name.clone(), format!("new-{}.jsonl", index + 1)))
        .collect();
    let mut normalizer = Normalizer {
        run,
        ids: HashMap::new(),
        renames,
    };
    let lines: Vec<Value> = run
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap_or_else(|_| Value::from(line)))
        .collect();
    let json_mode = !lines.is_empty() && lines.iter().all(Value::is_object);
    let stdout = if json_mode {
        let mut events: Vec<Value> = lines
            .into_iter()
            .map(|mut event| {
                match event["type"].as_str() {
                    Some("message_update") => {
                        if let Some(object) = event.as_object_mut() {
                            object.shift_remove("usage");
                        }
                    }
                    Some("message_start") if event["message"]["role"] == "assistant" => {
                        event["message"] = json!({"role": "assistant"});
                    }
                    Some("session") => {
                        event = json!({"type": "session", "version": event["version"]});
                    }
                    // JSON parser messages differ between implementations.
                    Some("response") if event["command"] == "parse" => {
                        event["error"] = json!("<parse error>");
                    }
                    _ => {}
                }
                normalizer.value(&event)
            })
            .collect();
        sort_tool_completions(&mut events);
        Value::Array(events)
    } else {
        Value::from(normalizer.text(&run.stdout))
    };
    let requests: Vec<Value> = run
        .requests
        .iter()
        .map(|request| {
            let body = serde_json::from_str::<Value>(&request.body)
                .unwrap_or_else(|_| Value::from(request.body.clone()));
            json!({
                "method": request.method,
                "path": request.path,
                "query": request.query,
                "body": normalizer.value(&body),
            })
        })
        .collect();
    let mut sessions: Vec<Value> = run
        .sessions
        .iter()
        .map(|file| {
            let lines: Vec<Value> = file
                .content
                .lines()
                .map(|line| {
                    let value =
                        serde_json::from_str::<Value>(line).unwrap_or_else(|_| Value::from(line));
                    normalizer.value(&value)
                })
                .collect();
            json!({"file": normalizer.text(&file.name), "lines": lines})
        })
        .collect();
    sessions.sort_by(|a, b| a["file"].to_string().cmp(&b["file"].to_string()));
    let stderr = normalizer.text(&run.stderr);
    let mut out = json!({
        "exitCode": run.exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "requests": requests,
    });
    if !sessions.is_empty() {
        out["sessions"] = Value::Array(sessions);
    }
    if let Some(screen) = &run.screen {
        out["screen"] = Value::from(normalize_screen(screen, &mut normalizer));
    }
    if let Some(progress) = &run.progress {
        out["progress"] = Value::from(progress.clone());
    }
    out
}

/// Screen rows without the product-specific startup header (pi's logo, its
/// docs tip, yapi's wordmark), with paths and session ids masked, trailing space
/// trimmed and blank runs collapsed, so the rest compares across programs.
fn normalize_screen(rows: &[String], normalizer: &mut Normalizer<'_>) -> Vec<String> {
    // The compact header ends with the "Press ... to show full startup help"
    // row; pi follows it with a docs tip that yapi lacks. Rows up to the tip's
    // end, or the part of the tip at the top of the screen, are dropped.
    const TIP: &str =
        "Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.";
    let start = match rows
        .iter()
        .position(|row| row.contains("to show full startup help"))
    {
        Some(press) => {
            let mut next = press + 1;
            while next < rows.len() && rows[next].trim().is_empty() {
                next += 1;
            }
            if rows
                .get(next)
                .is_some_and(|row| row.contains("Pi can explain"))
            {
                while next < rows.len() && !rows[next].trim().is_empty() {
                    next += 1;
                }
            }
            next
        }
        None => rows
            .iter()
            .take_while(|row| !row.trim().is_empty() && TIP.contains(row.trim()))
            .count(),
    };
    // pi's sign-in help lists two documents in its install, one per row; yapi
    // links its README in one row. Both become `<sign-in help>`.
    let mut merged: Vec<String> = Vec::new();
    let mut rest = rows[start..].iter().peekable();
    while let Some(row) = rest.next() {
        if row
            .trim_end()
            .ends_with("/pi-coding-agent/docs/providers.md")
            && rest
                .peek()
                .is_some_and(|next| next.trim_end().ends_with("/pi-coding-agent/docs/models.md"))
        {
            rest.next();
            let indent = &row[..row.len() - row.trim_start().len()];
            merged.push(format!(
                "{indent}https://github.com/SkymanOne/ri#models-and-sign-in"
            ));
        } else {
            merged.push(row.clone());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for row in &merged {
        if row.contains("▀▀█") || row.contains("▀▄▄▀ v") || row.starts_with(" yapi v")
        {
            continue;
        }
        // The expanded header shows the tip after the key help.
        if row.trim() == TIP {
            continue;
        }
        let mut row = normalizer
            .text(row.trim_end())
            .replacen("█▀ █ ", "", 1)
            .replacen("▄▀   ", "", 1);
        // yapi has no cache warming (docs/compat.md), so its status differs.
        if row.starts_with(" Status: Inactive (") {
            row = " Status: Inactive (<reason>)".to_owned();
        }
        let row = row
            .split(' ')
            .map(|word| if UUID.is_match(word) { "<uuid>" } else { word })
            .collect::<Vec<_>>()
            .join(" ");
        // A codemode call row (`✓ read {...} 12ms`) with its duration masked.
        let row = CALL_DURATION.replace(&row, "$1 <duration>").into_owned();
        if row.trim().is_empty() && out.last().is_none_or(|last| last.trim().is_empty()) {
            continue;
        }
        out.push(row);
    }
    while out.last().is_some_and(|last| last.trim().is_empty()) {
        out.pop();
    }
    out
}

fn sort_tool_completions(events: &mut [Value]) {
    let is_completion = |event: &Value| {
        matches!(
            event["type"].as_str(),
            Some("tool_execution_end" | "tool_execution_update")
        )
    };
    for run in events.chunk_by_mut(|a, b| is_completion(a) == is_completion(b)) {
        if is_completion(&run[0]) {
            run.sort_by_key(|event| event.to_string());
        }
    }
}

/// The session directory name pi derives from a working directory.
fn encoded_dir(cwd: &Path) -> String {
    let text = cwd.to_string_lossy();
    let trimmed = text.trim_start_matches(['/', '\\']);
    format!("--{}--", trimmed.replace(['/', '\\', ':'], "-"))
}

/// The first difference between two normalized runs, as a readable path.
pub fn first_difference(expected: &Value, actual: &Value) -> Option<String> {
    fn walk(path: &str, a: &Value, b: &Value) -> Option<String> {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                for (key, value) in x {
                    let child = format!("{path}.{key}");
                    match y.get(key) {
                        None => return Some(format!("{child}: missing in actual")),
                        Some(other) => {
                            if let Some(diff) = walk(&child, value, other) {
                                return Some(diff);
                            }
                        }
                    }
                }
                y.keys()
                    .find(|key| !x.contains_key(*key))
                    .map(|key| format!("{path}.{key}: unexpected in actual"))
            }
            (Value::Array(x), Value::Array(y)) => {
                for (index, (left, right)) in x.iter().zip(y).enumerate() {
                    if let Some(diff) = walk(&format!("{path}[{index}]"), left, right) {
                        return Some(diff);
                    }
                }
                (x.len() != y.len()).then(|| format!("{path}: length {} vs {}", x.len(), y.len()))
            }
            (Value::Number(x), Value::Number(y)) if x.as_f64() == y.as_f64() => None,
            _ if a == b => None,
            _ => Some(format!("{path}: expected {a}, got {b}")),
        }
    }
    walk("$", expected, actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_have_a_fixed_length() {
        for name in ["a", "tui-trust-command-save", &"x".repeat(80)] {
            let root = scratch_dir(name);
            assert_eq!(
                root.to_string_lossy().chars().count(),
                ROOT_LEN,
                "{}",
                root.display()
            );
        }
    }
}
