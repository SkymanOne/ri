//! pi golden fixtures (`tests/fixtures/pi`) must round-trip byte-identically, and the
//! typed views must cover every field pi writes. See the fixtures README.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::fs;
use std::path::{Path, PathBuf};

use ri_types::auth::AuthFile;
use ri_types::config::ConfigFile;
use ri_types::json;
use ri_types::message::Message;
use ri_types::models::ModelsConfig;
use ri_types::session::FileEntry;
use ri_types::settings::Settings;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn fixtures(dir: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/pi")
        .join(dir);
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {}", dir.display());
    paths
}

fn config_kind(path: &Path) -> ConfigFile {
    match path.file_name().unwrap().to_str().unwrap() {
        "settings.json" | "settings-all-fields.json" => ConfigFile::Settings,
        "auth.json" => ConfigFile::Auth,
        "models.json" => ConfigFile::Models,
        "keybindings.json" => ConfigFile::Keybindings,
        "mcp.json" => ConfigFile::Mcp,
        other => panic!("unknown config fixture {other}"),
    }
}

/// Session lines: each JSONL line parsed and written back must equal the original.
#[test]
fn sessions_round_trip_byte_identical() {
    for path in fixtures("sessions").into_iter().chain(fixtures("legacy")) {
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.ends_with('\n'),
            "{}: missing final newline",
            path.display()
        );
        for (index, line) in text.split_terminator('\n').enumerate() {
            let document: Value = serde_json::from_str(line).unwrap();
            assert_same(
                &json::to_string(&document).unwrap(),
                line,
                &format!("{}:{}", path.display(), index + 1),
            );
        }
    }
}

/// Config files: parsed and rendered with pi's layout must equal the original.
#[test]
fn configs_round_trip_byte_identical() {
    for path in fixtures("agent").into_iter().chain(fixtures("project")) {
        let text = fs::read_to_string(&path).unwrap();
        let document: Value = serde_json::from_str(&text).unwrap();
        assert_same(
            &config_kind(&path).render(&document).unwrap(),
            &text,
            &path.display().to_string(),
        );
    }
}

/// Every session line decodes into [`FileEntry`] without losing or changing a field.
#[test]
fn session_views_cover_fixtures() {
    for path in fixtures("sessions") {
        let text = fs::read_to_string(&path).unwrap();
        for (index, line) in text.split_terminator('\n').enumerate() {
            let document: Value = serde_json::from_str(line).unwrap();
            assert_covers::<FileEntry>(&document, &format!("{}:{}", path.display(), index + 1));
        }
    }
}

/// New objects built from the typed views use pi's key order. pi places some
/// assistant-message fields and compaction checkpoints by code path, so those are
/// exempt; see the session-format research in the crate docs.
#[test]
fn typed_views_use_pi_key_order() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pi/sessions/main.jsonl"),
    )
    .unwrap();
    for (index, line) in text.split_terminator('\n').enumerate() {
        let entry: FileEntry = serde_json::from_str(line).unwrap();
        let path_dependent = match &entry {
            FileEntry::Message(entry) => matches!(entry.message, Message::Assistant(_)),
            FileEntry::Compaction(_) => true,
            _ => false,
        };
        if !path_dependent {
            assert_same(
                &json::to_string(&entry).unwrap(),
                line,
                &format!("main.jsonl:{}", index + 1),
            );
        }
    }
}

/// Settings, auth and models files decode into their views without losing a field.
/// keybindings.json and mcp.json have no typed view yet.
#[test]
fn config_views_cover_fixtures() {
    for path in fixtures("agent").into_iter().chain(fixtures("project")) {
        let document: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let location = path.display().to_string();
        match config_kind(&path) {
            ConfigFile::Settings => assert_covers::<Settings>(&document, &location),
            ConfigFile::Auth => assert_covers::<AuthFile>(&document, &location),
            ConfigFile::Models => assert_covers::<ModelsConfig>(&document, &location),
            ConfigFile::Keybindings | ConfigFile::Mcp => {}
        }
    }
}

fn assert_same(actual: &str, expected: &str, location: &str) {
    if actual != expected {
        let at = actual
            .bytes()
            .zip(expected.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let context = |s: &str| {
            s.get(at.saturating_sub(40)..(at + 40).min(s.len()))
                .unwrap_or("")
                .to_owned()
        };
        panic!(
            "{location}: differs at byte {at}\n  expected: …{}…\n  actual:   …{}…",
            context(expected),
            context(actual)
        );
    }
}

/// Decodes `document` as `T` and checks that encoding it again yields the same JSON,
/// ignoring key order and integer-versus-float representation.
fn assert_covers<T: DeserializeOwned + Serialize>(document: &Value, location: &str) {
    let typed: T = serde_json::from_value(document.clone())
        .unwrap_or_else(|err| panic!("{location}: does not decode: {err}"));
    let encoded = serde_json::to_value(&typed).unwrap();
    if let Some(path) = first_difference(&normalize(document), &normalize(&encoded), String::new())
    {
        panic!("{location}: typed view differs at `{path}`");
    }
}

fn normalize(value: &Value) -> Value {
    match value {
        Value::Number(number) => serde_json::json!(number.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), normalize(v))).collect())
        }
        other => other.clone(),
    }
}

fn first_difference(expected: &Value, actual: &Value, path: String) -> Option<String> {
    match (expected, actual) {
        (Value::Object(a), Value::Object(b)) => a
            .keys()
            .chain(b.keys().filter(|key| !a.contains_key(*key)))
            .find_map(|key| match (a.get(key), b.get(key)) {
                (Some(x), Some(y)) => first_difference(x, y, format!("{path}.{key}")),
                (Some(_), None) => Some(format!("{path}.{key} (dropped)")),
                (None, _) => Some(format!("{path}.{key} (added)")),
            }),
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
            .iter()
            .zip(b)
            .enumerate()
            .find_map(|(i, (x, y))| first_difference(x, y, format!("{path}[{i}]"))),
        _ if expected == actual => None,
        _ => Some(format!("{path} ({expected} vs {actual})")),
    }
}
