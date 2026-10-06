//! pi's example extensions register the same tools, commands, flags,
//! shortcuts and event handlers in yapi as in pi.
//!
//! pi's side is `tests/fixtures/pi/extensions/registrations.json`, written by
//! `tests/fixtures/pi/generator/extensions.mjs`. The examples come from the
//! generator's npm install of pi; without it the test only reports that.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use yapi_ext::{Engine, Instance, NoBridge, Options};

/// The share of examples that must match. AGENTS.md's M5 exit criterion asks
/// for 90%, and the documentation states that every example matches.
const REQUIRED: f64 = 1.0;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// pi's dump of one loaded extension.
fn dump(loaded: &Value) -> Value {
    let list = |key: &str| loaded[key].as_array().cloned().unwrap_or_default();
    json!({
        "tools": list("tools").iter().map(|tool| json!({
            "name": tool["name"],
            "label": tool["label"],
            "description": tool["description"],
            "parameters": tool["parameters"],
        })).collect::<Vec<_>>(),
        "commands": list("commands").iter().map(|command| json!({
            "name": command["name"],
            "description": command["description"],
        })).collect::<Vec<_>>(),
        "flags": list("flags"),
        "shortcuts": list("shortcuts"),
        "events": list("events"),
    })
}

/// pi's `discoverAndLoadExtensions` for one path, with empty project and agent
/// directories: a file, a directory's entry points, or else its extension
/// files and its subdirectories' entry points, by name.
fn discover(path: &Path) -> Vec<PathBuf> {
    use yapi_core::extensions::discovery::entries;
    if !path.is_dir() {
        return vec![path.to_owned()];
    }
    if let Some(found) = entries(path) {
        return found;
    }
    let mut listed: Vec<_> = std::fs::read_dir(path).unwrap().flatten().collect();
    listed.sort_by_key(std::fs::DirEntry::file_name);
    let mut found = Vec::new();
    for entry in listed {
        let (path, kind) = (entry.path(), entry.file_type().unwrap());
        let name = entry.file_name().to_string_lossy().into_owned();
        if (kind.is_file() || kind.is_symlink())
            && [".ts", ".js", ".wasm"]
                .iter()
                .any(|ext| name.ends_with(ext))
        {
            found.push(path);
        } else if kind.is_dir() || kind.is_symlink() {
            found.extend(entries(&path).unwrap_or_default());
        }
    }
    found
}

async fn load_example(
    engine: Engine,
    examples: PathBuf,
    name: String,
    scratch: PathBuf,
) -> (String, Value) {
    let cwd = scratch.join("project");
    let agent_dir = scratch.join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let paths = discover(&examples.join(&name));
    let mut options = Options::new(cwd.clone());
    options.agent_dir = agent_dir.clone();
    let instance = Instance::start(&engine, options, Arc::new(NoBridge))
        .await
        .unwrap();
    let extensions: Vec<Value> = paths
        .iter()
        .enumerate()
        .map(|(id, path)| json!({"id": id, "path": path}))
        .collect();
    let loaded = instance
        .call("load", &json!({"cwd": cwd, "extensions": extensions}))
        .await;
    let mut result = json!({"extensions": [], "errors": []});
    // As the generator does, so descriptions naming these directories compare.
    let normalize = |value: Value| -> Value {
        let text = yapi_types::json::to_string(&value)
            .unwrap()
            .replace(&*agent_dir.to_string_lossy(), "<agent>")
            .replace(&*cwd.to_string_lossy(), "<cwd>")
            // yapi's project directory is `.yapi`, by design (docs/compat.md).
            .replace(".yapi/", ".pi/");
        serde_json::from_str(&text).unwrap()
    };
    match loaded {
        Ok(loaded) => {
            for extension in loaded["extensions"].as_array().cloned().unwrap_or_default() {
                match extension["error"].as_str() {
                    Some(error) => {
                        let error = error.replace(&*examples.to_string_lossy(), "<examples>");
                        let first = error.lines().next().unwrap_or_default().to_owned();
                        result["errors"]
                            .as_array_mut()
                            .unwrap()
                            .push(Value::String(first));
                    }
                    None => result["extensions"]
                        .as_array_mut()
                        .unwrap()
                        .push(normalize(dump(&extension))),
                }
            }
        }
        Err(err) => result["errors"] = json!([format!("instance: {err}")]),
    }
    (name, result)
}

#[tokio::test(flavor = "multi_thread")]
async fn example_registrations_match_pi() {
    let root = workspace();
    let examples = root.join("tests/fixtures/pi/generator/node_modules/@earendil-works/pi-coding-agent/examples/extensions");
    if !examples.is_dir() {
        eprintln!("skipped: pi is not installed in tests/fixtures/pi/generator (npm ci there)");
        return;
    }
    let examples = examples.canonicalize().unwrap();
    let expected: BTreeMap<String, Value> = serde_json::from_str(
        &std::fs::read_to_string(root.join("tests/fixtures/pi/extensions/registrations.json"))
            .unwrap(),
    )
    .unwrap();
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let engine = Engine::new(Some(&tmp.join("wasm-cache"))).unwrap();
    let scratch = tmp.join("examples");
    let _ = std::fs::remove_dir_all(&scratch);

    let mut tasks = tokio::task::JoinSet::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(8));
    for name in expected.keys() {
        let (engine, examples, name, scratch) = (
            engine.clone(),
            examples.clone(),
            name.clone(),
            scratch.join(name),
        );
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();
            load_example(engine, examples, name, scratch).await
        });
    }
    let mut actual = BTreeMap::new();
    while let Some(result) = tasks.join_next().await {
        let (name, value) = result.unwrap();
        actual.insert(name, value);
    }
    std::fs::write(
        tmp.join("registrations-yapi.json"),
        yapi_types::json::to_string_pretty(&actual, "\t").unwrap(),
    )
    .unwrap();

    let mut mismatched = Vec::new();
    for (name, want) in &expected {
        let got = &actual[name];
        if got != want {
            mismatched.push(name.clone());
            eprintln!("--- {name}\n  pi: {want}\n  yapi: {got}");
        }
    }
    let matched = expected.len() - mismatched.len();
    let share = matched as f64 / expected.len() as f64;
    eprintln!(
        "{matched} of {} examples match pi ({:.1}%); differ: {mismatched:?}",
        expected.len(),
        share * 100.0
    );
    assert!(
        share >= REQUIRED,
        "{matched} of {} examples match pi",
        expected.len()
    );
}
