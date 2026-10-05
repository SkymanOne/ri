//! Session files against pi's fixtures and pi's context output.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};

use serde_json::Value;
use yapi_core::session::{SessionManager, list};
use yapi_types::message::{Content, Message, UserMessage};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pi")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("yapi-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The value as JavaScript sees it: numbers re-read from their JS text form.
fn js(value: &Value) -> Value {
    serde_json::from_str(&yapi_types::json::to_string(value).unwrap()).unwrap()
}

#[test]
fn contexts_match_pi() {
    let scratch = Scratch::new("contexts");
    for entry in std::fs::read_dir(fixtures().join("sessions")).unwrap() {
        let source = entry.unwrap().path();
        let name = source.file_stem().unwrap().to_string_lossy().into_owned();
        // Opening may append a final newline; work on a copy.
        let copy = scratch.0.join(source.file_name().unwrap());
        std::fs::copy(&source, &copy).unwrap();
        let session = SessionManager::open(&copy, None, Some(Path::new("/tmp"))).unwrap();
        let context = session.build_context();
        let expected: Value = serde_json::from_str(
            &std::fs::read_to_string(fixtures().join("contexts").join(format!("{name}.json")))
                .unwrap(),
        )
        .unwrap();
        let actual = serde_json::json!({
            "messages": serde_json::to_value(&context.messages).unwrap(),
            "thinkingLevel": context.thinking_level,
            "model": context.model.as_ref().map(|(provider, model_id)| serde_json::json!({"provider": provider, "modelId": model_id})),
        });
        assert_eq!(js(&actual), expected, "context of {name}");
        // Reading never rewrites a current-version file.
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            std::fs::read(&source).unwrap(),
            "{name} changed on open"
        );
    }
}

#[test]
fn migrates_v1_files_like_pi() {
    let scratch = Scratch::new("migrate");
    let copy = scratch.0.join("old.jsonl");
    std::fs::copy(fixtures().join("legacy/large-session.v1.jsonl"), &copy).unwrap();
    let session = SessionManager::open(&copy, None, None).unwrap();
    let text = std::fs::read_to_string(&copy).unwrap();
    let header: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(header["version"], 3);
    // Every entry got an id and a parent chain, in file order.
    let mut previous = Value::Null;
    for line in text.lines().skip(1) {
        let entry: Value = serde_json::from_str(line).unwrap();
        assert_eq!(entry["id"].as_str().unwrap().len(), 8);
        assert_eq!(entry["parentId"], previous);
        previous = entry["id"].clone();
    }
    // The migrated context matches pi's migration of the same excerpt.
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("contexts/legacy-large-session.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        js(&serde_json::to_value(&session.build_context().messages).unwrap()),
        expected["messages"]
    );
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: Content::Text(text.into()),
        timestamp: 1,
    })
}

#[test]
fn creates_files_lazily_and_branches() {
    let scratch = Scratch::new("lazy");
    let mut session = SessionManager::create(Path::new("/work"), &scratch.0, None).unwrap();
    session.append_model_change("anthropic", "m").unwrap();
    let file = session.file().unwrap().to_path_buf();
    assert!(!file.exists(), "no file before a conversation");
    let first = session.append_message(user("one")).unwrap();
    assert!(file.exists());
    session.append_message(user("two")).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 4);

    session.branch(&first).unwrap();
    session.append_message(user("three")).unwrap();
    let texts: Vec<String> = session
        .build_context()
        .messages
        .iter()
        .map(|message| match message {
            Message::User(user) => user.content.text(""),
            _ => String::new(),
        })
        .collect();
    assert_eq!(texts, ["one", "three"]);

    // A file written by yapi reopens with the same tree.
    let reopened = SessionManager::open(&file, None, None).unwrap();
    assert_eq!(reopened.build_context(), session.build_context());
    assert_eq!(reopened.id(), session.id());
    assert_eq!(list(&scratch.0, None)[0].first_message, "one");

    // Branching off into a new file keeps only the path.
    let branched = session.create_branched_session(&first).unwrap().unwrap();
    let lines = std::fs::read_to_string(&branched).unwrap().lines().count();
    assert_eq!(lines, 3, "header, model change, first message");
}
