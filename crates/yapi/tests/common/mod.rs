//! Helpers shared by the binary's tests.
#![allow(
    clippy::unwrap_used,
    dead_code,
    reason = "test helpers; each test file uses some"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The repository root.
pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// `tests/fixtures`.
pub fn fixtures() -> PathBuf {
    repo().join("tests/fixtures")
}

/// A new empty directory for one test, in the system's temporary directory:
/// tests set `HOME` to such a directory, so an ancestor in the real home
/// (whose `.agents/skills` asks for project trust, as in pi) would change
/// what yapi does.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("yapi-tests-{}", std::process::id()))
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The binary in `home`, with a cleared environment whose home is `home` and
/// whose agent directory is `home/agent`.
pub fn yapi(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yapi"));
    command
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("YAPI_CODING_AGENT_DIR", home.join("agent"));
    command
}

/// The commands `yapi --mode rpc` started by `command` reports, failing on
/// an extension error.
pub fn rpc_commands(command: &mut Command) -> serde_json::Value {
    let mut child = command
        .args(["--mode", "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"id":"commands","type":"get_commands"}}"#).unwrap();
    let mut commands = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let event: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        assert_ne!(event["type"], "extension_error", "{event}");
        if event["type"] == "response" && event["id"] == "commands" {
            commands = Some(event["data"]["commands"].clone());
            break;
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    commands.expect("no get_commands response")
}
