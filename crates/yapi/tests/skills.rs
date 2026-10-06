//! The agent skills in `skills/`: they load as yapi loads skills, their links
//! resolve, their templates register what the skills describe, and their
//! scripts work against this binary. The script tests need `python3`.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use yapi_ext::{Engine, Instance, NoBridge, Options};
use yapi_mock::{Cassette, MockServer};
use yapi_types::rpc::SourceInfo;

fn skill(path: &str) -> PathBuf {
    common::repo().join("skills").join(path)
}

fn scratch(name: &str) -> PathBuf {
    common::scratch(&format!("skills-{name}"))
}

/// The relative link targets in a Markdown file, without anchors.
fn relative_links(text: &str) -> Vec<String> {
    text.split("](")
        .skip(1)
        .filter_map(|rest| rest.split_once(')').map(|(target, _)| target))
        .filter(|target| !target.contains("://") && !target.starts_with('#'))
        .map(|target| target.split('#').next().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn skills_load_cleanly_and_their_links_resolve() {
    let source = SourceInfo {
        path: common::repo().join("skills").display().to_string(),
        source: "local".into(),
        scope: "user".into(),
        origin: "top-level".into(),
        base_dir: None,
    };
    let (mut skills, diagnostics) = yapi_core::resources::skills_from(&[source]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, ["yapi", "yapi-extension"]);
    for skill in &skills {
        assert_eq!(
            skill.base_dir.file_name().unwrap().to_str(),
            Some(skill.name.as_str())
        );
        assert!(skill.description.chars().count() <= 1024, "{}", skill.name);
        assert!(!skill.disable_model_invocation);
        for file in [
            skill.file_path.clone(),
            skill.base_dir.join("references/rpc.md"),
            skill.base_dir.join("references/differences.md"),
        ] {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for link in relative_links(&text) {
                let target = file.parent().unwrap().join(&link);
                assert!(target.exists(), "{}: {link}", file.display());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_extension_template_registers_what_the_skill_describes() {
    let dir = scratch("template");
    let engine = Engine::new(Some(&dir.join("wasm-cache"))).unwrap();
    let instance = Instance::start(&engine, Options::new(dir.clone()), Arc::new(NoBridge))
        .await
        .unwrap();
    let template = skill("yapi-extension/templates/pi-extension.ts");
    let loaded = instance
        .call(
            "load",
            &json!({"cwd": dir, "extensions": [{"id": 1, "path": template}]}),
        )
        .await
        .unwrap();
    let extension = &loaded["extensions"][0];
    assert!(extension.get("error").is_none(), "{extension}");
    assert_eq!(extension["tools"][0]["name"], "shout");
    assert_eq!(extension["commands"][0]["name"], "hello");
    assert_eq!(extension["flags"][0]["name"], "shout-suffix");
    assert_eq!(extension["events"], json!(["tool_call"]));
}

/// `command`'s output, or a panic after two minutes, so that a script that
/// deadlocks fails the test instead of hanging it.
fn output(mut command: Command) -> Output {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(child.wait_with_output().unwrap()));
    receive
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            panic!("{command:?} did not finish");
        })
}

fn python(script: &str, yapi: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new("python3");
    command
        .arg(skill(script))
        .arg("--yapi")
        .arg(yapi)
        .args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    output(command)
}

fn yapi() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_yapi"))
}

#[test]
fn check_extension_reports_commands_and_load_errors() {
    let dir = scratch("check");
    let template = skill("yapi-extension/templates/pi-extension.ts");
    let output = python(
        "yapi-extension/scripts/check-extension.py",
        yapi(),
        &[template.to_str().unwrap()],
        &[("HOME", &dir)],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.starts_with("Loaded 1 extension(s).\n"), "{stdout}");
    assert!(stdout.contains("  /hello: Says hello\n"), "{stdout}");

    let broken = dir.join("broken.ts");
    std::fs::write(
        &broken,
        "import missing from \"not-installed\";\nexport default function () { missing(); }\n",
    )
    .unwrap();
    let output = python(
        "yapi-extension/scripts/check-extension.py",
        yapi(),
        &[broken.to_str().unwrap()],
        &[("HOME", &dir)],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("not-installed"), "{stderr}");

    // An extension that loads but fails in a handler.
    let throws = dir.join("throws.ts");
    std::fs::write(
        &throws,
        "export default function (pi) {\n  pi.registerCommand(\"throws\", { description: \"Fails on start\", handler: async () => {} });\n  pi.on(\"session_start\", () => { throw new Error(\"boom\"); });\n}\n",
    )
    .unwrap();
    let output = python(
        "yapi-extension/scripts/check-extension.py",
        yapi(),
        &[throws.to_str().unwrap()],
        &[("HOME", &dir)],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        stdout,
        "Loaded with 1 error(s).\n  /throws: Fails on start\n"
    );
    assert!(stderr.contains("boom"), "{stderr}");
}

/// yapi's stderr fills a pipe after 64 KB, so the scripts must not leave it
/// unread while they wait for yapi's output.
#[test]
fn check_extension_survives_a_flood_of_logs() {
    let dir = scratch("check-logs");
    let noisy = dir.join("noisy.ts");
    std::fs::write(
        &noisy,
        "export default function (pi) {\n  console.error(\"x\".repeat(200000));\n  pi.registerCommand(\"noisy\", { description: \"Logs a lot\", handler: async () => {} });\n}\n",
    )
    .unwrap();
    let output = python(
        "yapi-extension/scripts/check-extension.py",
        yapi(),
        &[noisy.to_str().unwrap()],
        &[("HOME", &dir)],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert_eq!(stdout, "Loaded 1 extension(s).\n  /noisy: Logs a lot\n");
    assert!(output.stderr.len() > 200_000);
}

/// A stand-in for yapi that floods stderr, ends the run, and exits without
/// answering `get_last_assistant_text`.
#[cfg(unix)]
#[test]
fn ask_fails_when_yapi_stops_before_the_answer() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = scratch("ask-stops");
    let fake = dir.join("fake-yapi");
    std::fs::write(
        &fake,
        "#!/bin/sh\nread prompt\nhead -c 200000 /dev/zero | tr '\\0' x >&2\necho 'yapi crashed' >&2\necho '{\"type\":\"agent_end\"}'\nread answer\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = python("yapi/scripts/ask.py", &fake, &["Say hello"], &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.ends_with("yapi crashed\n"),
        "{}",
        &stderr[stderr.len().saturating_sub(200)..]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_prints_the_answer() {
    let cassette = Cassette::load(
        &common::repo().join("tests/fixtures/cassettes/anthropic-messages/text.json"),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = MockServer::start(addr, cassette).await.unwrap();
    let dir = scratch("ask");
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        json!({"providers": {"anthropic": {"baseUrl": server.url()}}}).to_string(),
    )
    .unwrap();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("python3")
            .arg(skill("yapi/scripts/ask.py"))
            .args(["--yapi", env!("CARGO_BIN_EXE_yapi")])
            .args([
                "--model",
                "anthropic/claude-sonnet-4-5",
                "Say hello",
                "--",
                "-ne",
            ])
            .current_dir(&dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &dir)
            .env("YAPI_CODING_AGENT_DIR", &agent)
            .env("PI_OFFLINE", "1")
            .env("ANTHROPIC_API_KEY", "mock")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Hello from the mock.\n"
    );
    assert_eq!(server.finish().unwrap().len(), 1);
}
