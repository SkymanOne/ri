//! `yapi new`: the native extension package it creates.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn scratch(name: &str) -> PathBuf {
    common::scratch(&format!("new-{name}"))
}

fn yapi_new(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yapi"))
        .arg("new")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn read(path: PathBuf) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Every file under `dir`, relative to it, in name order.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(dir).unwrap();
                found.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn creates_a_package_from_the_template() {
    let dir = scratch("package");
    let output = yapi_new(&dir, &["my-ext"]);
    assert!(output.status.success(), "{output:?}");
    let project = dir.join("my-ext");
    let template = common::repo().join("crates/yapi/templates/extension");
    // Cargo reads every manifest in a git dependency's repository, and fails
    // on a placeholder in a package name.
    assert!(!template.join("Cargo.toml").exists());
    // The template's files, without the one only `cargo generate` reads, and
    // without cargo-generate's `.liquid` suffix.
    let expected: Vec<String> = files(&template)
        .into_iter()
        .filter(|path| path != "cargo-generate.toml")
        .map(|path| path.strip_suffix(".liquid").unwrap_or(&path).to_owned())
        .collect();
    assert_eq!(files(&project), expected);
    for path in &expected {
        assert!(
            !read(project.join(path)).contains("{{"),
            "{path} keeps a placeholder"
        );
    }
    let cargo = read(project.join("Cargo.toml"));
    assert!(cargo.contains("name = \"my-ext\""));
    // The SDK at this release's tag, so a version bump needs a template bump.
    assert!(
        cargo.contains(concat!("tag = \"v", env!("CARGO_PKG_VERSION"), "\"")),
        "{cargo}"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&read(project.join("package.json"))).unwrap();
    assert_eq!(manifest["name"], "my-ext");
    assert_eq!(
        manifest["yapi"]["extensions"],
        serde_json::json!(["./extensions/my_ext.wasm"])
    );
    // `crates/yapi-ext/tests/native.rs` builds and runs this example.
    assert_eq!(
        read(project.join("src/lib.rs")),
        read(common::repo().join("guest/examples/hello/src/lib.rs"))
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("Created native extension package \"my-ext\" in my-ext\n"));
    assert!(stdout.contains("  cd my-ext\n"));
    assert!(stdout.contains("  cp target/wasm32-wasip2/release/my_ext.wasm extensions/\n"));
}

#[test]
fn names_the_package_with_name_and_fills_an_empty_directory() {
    let dir = scratch("named");
    let output = yapi_new(&dir, &[".", "--name", "tool_box"]);
    assert!(output.status.success(), "{output:?}");
    assert!(read(dir.join("Cargo.toml")).contains("name = \"tool_box\""));
    assert!(read(dir.join("package.json")).contains("./extensions/tool_box.wasm"));
    assert!(!String::from_utf8(output.stdout).unwrap().contains("  cd "));
}

#[test]
fn refuses_existing_files_and_invalid_names() {
    let dir = scratch("refused");
    std::fs::create_dir_all(dir.join("taken")).unwrap();
    std::fs::write(dir.join("taken/notes.txt"), "keep").unwrap();
    let output = yapi_new(&dir, &["taken"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: taken already exists\n"
    );
    assert_eq!(files(&dir.join("taken")), ["notes.txt"]);

    // npm rejects capitals and a leading `_`.
    for name in ["1st", "-dash", "_under", "Upper", "has space", "ünï"] {
        let output = yapi_new(&dir, &["fresh", "--name", name]);
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!(
                "Error: \"{name}\" is not a valid package name. Use lowercase ASCII letters, digits, - and _, starting with a letter.\n"
            )
        );
        assert!(!dir.join("fresh").exists(), "{name}");
    }
    let output = yapi_new(&dir, &[]);
    assert_eq!(output.status.code(), Some(1));
}

/// The package loads in yapi once the build is in `extensions/`. The `hello`
/// example's build stands in for `cargo build`, which needs the network.
#[test]
fn the_package_loads_once_built() {
    let dir = scratch("loads");
    assert!(yapi_new(&dir, &["greeter-ext"]).status.success());
    let project = dir.join("greeter-ext");
    std::fs::copy(
        common::repo().join("crates/yapi-ext/tests/fixtures/hello.wasm"),
        project.join("extensions/greeter_ext.wasm"),
    )
    .unwrap();
    let agent = dir.join("agent");
    let mut child = Command::new(env!("CARGO_BIN_EXE_yapi"))
        .args(["--mode", "rpc", "--no-session", "--offline", "-ne", "-e"])
        .arg(&project)
        .current_dir(&dir)
        .env_clear()
        .env("HOME", &dir)
        .env("YAPI_CODING_AGENT_DIR", &agent)
        .env("PI_OFFLINE", "1")
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
    let commands = commands.expect("no get_commands response");
    let hello = commands
        .as_array()
        .unwrap()
        .iter()
        .find(|command| command["name"] == "hello")
        .expect("the extension's /hello command");
    assert_eq!(hello["source"], "extension");
    assert_eq!(hello["description"], "Says hello");
}
