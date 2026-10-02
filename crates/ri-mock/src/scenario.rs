//! End-to-end scenarios: run pi or ri as a black box against a cassette, and
//! normalize what they print and send so the two can be compared.
//!
//! A scenario is a cassette, command-line arguments and files to create in a fresh
//! working directory. The agent directory gets a `models.json` pointing the
//! providers at the mock server; credentials are dummy environment variables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use indexmap::IndexMap;
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
}

/// Which program runs a scenario.
#[derive(Clone, Debug)]
pub enum Program {
    /// pi, at this executable.
    Pi(PathBuf),
    /// ri, at this executable.
    Ri(PathBuf),
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
    let path = fixtures_dir().join("scenarios/scenarios.json");
    let text = std::fs::read_to_string(&path).map_err(|source| Error::Read {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| Error::Parse { path, source })
}

fn scratch_dir(name: &str) -> PathBuf {
    static COUNTER: OnceLock<std::sync::atomic::AtomicU64> = OnceLock::new();
    let n = COUNTER
        .get_or_init(Default::default)
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("ri-scenario-{}-{n}-{name}", std::process::id()))
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
    let files = scenario
        .files
        .iter()
        .map(|(relative, content)| (cwd.join(relative), content))
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
            .replace("{{agent}}", &agent_dir.to_string_lossy());
        std::fs::write(&path, content).map_err(io(&path))?;
        seeded.push(path);
    }
    let models = json!({"providers": {
        "anthropic": {"baseUrl": url},
        "groq": {"baseUrl": url},
        "openai": {"baseUrl": format!("{url}/v1")},
        "google": {"baseUrl": format!("{url}/v1beta")},
    }});
    let models_path = agent_dir.join("models.json");
    std::fs::write(&models_path, models.to_string()).map_err(io(&models_path))?;

    let (executable, dir_var) = match program {
        Program::Pi(path) => (path, "PI_CODING_AGENT_DIR"),
        Program::Ri(path) => (path, "RI_CODING_AGENT_DIR"),
    };
    let mut command = tokio::process::Command::new(executable);
    // A clean environment: ambient credentials on the host must not change what
    // either program sees.
    command
        .args(&scenario.args)
        .current_dir(&cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(dir_var, &agent_dir)
        .env("HOME", &root)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("ANTHROPIC_API_KEY", "mock")
        .env("GROQ_API_KEY", "mock")
        .env("OPENAI_API_KEY", "mock")
        .env("GEMINI_API_KEY", "mock")
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
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        requests,
        cwd: cwd.clone(),
        agent_dir,
        sessions,
    };
    let _ = std::fs::remove_dir_all(&root);
    Ok(run)
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
    "parentId",
    "targetId",
    "fromId",
    "firstKeptEntryId",
    "responseId",
    "toolCallId",
    "prompt_cache_key",
];

impl Normalizer<'_> {
    /// Text normalization: paths, new session file names, ri's name in the
    /// prompt, pi's docs section.
    fn text(&self, text: &str) -> String {
        let mut text = text
            .replace(
                &self.run.agent_dir.to_string_lossy().into_owned(),
                "<agent>",
            )
            .replace(&self.run.cwd.to_string_lossy().into_owned(), "<cwd>")
            .replace("operating inside ri,", "operating inside pi,");
        for (from, to) in &self.renames {
            text = text.replace(from, to);
        }
        // pi's documentation section points into pi's install; ri has none.
        while let Some(start) = text.find("\n\n<docs>\n") {
            match text[start..].find("\n</docs>") {
                Some(end) => text.replace_range(start..start + end + "\n</docs>".len(), ""),
                None => break,
            }
        }
        text
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
                            "timestamp" | "estimatedTokensAfter",
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
    let mut out = json!({"exitCode": run.exit_code, "stdout": stdout, "requests": requests});
    if !sessions.is_empty() {
        out["sessions"] = Value::Array(sessions);
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
    let key = |event: &Value| event.to_string();
    let mut start = 0;
    while start < events.len() {
        if !is_completion(&events[start]) {
            start += 1;
            continue;
        }
        let end = (start..events.len())
            .find(|&index| !is_completion(&events[index]))
            .unwrap_or(events.len());
        events[start..end].sort_by_key(key);
        start = end;
    }
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
