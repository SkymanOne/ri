//! Built-in tools on a scratch directory.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ri_agent::UpdateSink;
use ri_core::tools::{ToolEnv, builtin};
use ri_types::event::ToolResult;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("ri-tools-{name}-{}", std::process::id()));
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

fn env(cwd: &Path) -> ToolEnv {
    ToolEnv {
        cwd: cwd.to_path_buf(),
        runtime: Arc::default(),
        bin_dir: cwd.join("bin"),
    }
}

fn sink() -> (UpdateSink, Arc<Mutex<Vec<ToolResult>>>) {
    let updates = Arc::new(Mutex::new(Vec::new()));
    let store = updates.clone();
    (
        Arc::new(move |update| store.lock().unwrap().push(update)),
        updates,
    )
}

async fn call(cwd: &Path, name: &str, args: Value) -> Result<ToolResult, String> {
    let tool = builtin(name, &env(cwd)).unwrap().tool;
    let (updates, _) = sink();
    tool.execute("id".into(), args, CancellationToken::new(), updates)
        .await
}

fn text(result: &ToolResult) -> String {
    ri_types::message::blocks_text(&result.content, "\n")
}

#[tokio::test]
async fn read_pages_through_files() {
    let dir = Scratch::new("read");
    let lines: Vec<String> = (1..=5).map(|n| format!("line {n}")).collect();
    std::fs::write(dir.0.join("a.txt"), lines.join("\n")).unwrap();

    let result = call(
        &dir.0,
        "read",
        json!({"path": "a.txt", "offset": 2, "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        "line 2\nline 3\n\n[2 more lines in file. Use offset=4 to continue.]"
    );
    let err = call(&dir.0, "read", json!({"path": "a.txt", "offset": 9}))
        .await
        .unwrap_err();
    assert_eq!(err, "Offset 9 is beyond end of file (5 lines total)");
    let err = call(&dir.0, "read", json!({"path": "missing.txt"}))
        .await
        .unwrap_err();
    assert!(err.starts_with("ENOENT: no such file or directory, access '"));

    let long: Vec<String> = (1..=2500).map(|n| n.to_string()).collect();
    std::fs::write(dir.0.join("long.txt"), long.join("\n")).unwrap();
    let result = call(&dir.0, "read", json!({"path": "@long.txt"}))
        .await
        .unwrap();
    assert!(
        text(&result).ends_with("\n\n[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]")
    );
    assert_eq!(
        result.details.unwrap()["truncation"]["truncatedBy"],
        "lines"
    );
}

#[tokio::test]
async fn write_and_edit_preserve_bom_and_crlf() {
    let dir = Scratch::new("edit");
    let result = call(
        &dir.0,
        "write",
        json!({"path": "sub/f.txt", "content": "\u{feff}one\r\ntwo\r\n"}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), "Successfully wrote to sub/f.txt");

    let result = call(
        &dir.0,
        "edit",
        json!({"path": "sub/f.txt", "edits": [{"oldText": "two\n", "newText": "2\n"}]}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        "Successfully replaced 1 block(s) in sub/f.txt."
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("sub/f.txt")).unwrap(),
        "\u{feff}one\r\n2\r\n"
    );
    let details = result.details.unwrap();
    assert_eq!(details["diff"], " 1 one\n-2 two\n+2 2");
    assert_eq!(details["firstChangedLine"], 2);

    let err = call(
        &dir.0,
        "edit",
        json!({"path": "nope.txt", "edits": [{"oldText": "a", "newText": "b"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "Could not edit file: nope.txt. Error code: ENOENT.");

    // Node's messages for a directory: `read` names no path, `open` does.
    std::fs::create_dir(dir.0.join("adir")).unwrap();
    let err = call(&dir.0, "write", json!({"path": "adir", "content": "x"}))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        format!(
            "EISDIR: illegal operation on a directory, open '{}'",
            dir.0.join("adir").display()
        )
    );
}

#[tokio::test]
async fn edit_accepts_legacy_arguments() {
    let dir = env(Path::new("/"));
    let tool = builtin("edit", &dir).unwrap().tool;
    let args = json!({"path": "f", "oldText": "a", "newText": "b"});
    let prepared = tool.prepare_arguments(args.as_object().unwrap().clone());
    assert_eq!(
        Value::Object(prepared),
        json!({"path": "f", "edits": [{"oldText": "a", "newText": "b"}]})
    );
}

#[tokio::test]
async fn bash_reports_output_and_exit_codes() {
    let dir = Scratch::new("bash");
    let result = call(&dir.0, "bash", json!({"command": "echo hi; echo err >&2"}))
        .await
        .unwrap();
    // stdout and stderr are separate pipes, so their lines may arrive in either order.
    let mut lines: Vec<String> = text(&result).lines().map(str::to_owned).collect();
    lines.sort();
    assert_eq!(lines, ["err", "hi"]);
    assert_eq!(result.structured_content.as_ref().unwrap()["exit_code"], 0);

    let result = call(&dir.0, "bash", json!({"command": "exit 3"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "(no output)\n\nCommand exited with code 3");
    assert_eq!(result.is_error, Some(true));

    let err = call(
        &dir.0,
        "bash",
        json!({"command": "echo start; sleep 5", "timeout": 0.2}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "start\n\n\nCommand timed out after 0.2 seconds");
}

#[tokio::test]
async fn bash_aborts_and_streams_updates() {
    let dir = Scratch::new("abort");
    let tool = builtin("bash", &env(&dir.0)).unwrap().tool;
    let (updates, seen) = sink();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let err = tool
        .execute(
            "id".into(),
            json!({"command": "echo partial; sleep 10"}),
            cancel,
            updates,
        )
        .await
        .unwrap_err();
    assert_eq!(err, "partial\n\n\nCommand aborted");
    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 2, "expected an initial and an output update");
    assert_eq!(text(&seen[1]), "partial\n");
}
