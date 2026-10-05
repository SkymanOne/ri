//! The agent skills in `skills/`: they load as yapi loads skills, their links
//! resolve, their templates register what the skills describe, and their
//! scripts work against this binary. The script tests need `python3`.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use serde_json::json;
use yapi_ext::{Engine, Instance, NoBridge, Options};
use yapi_mock::{Cassette, MockServer};
use yapi_types::rpc::SourceInfo;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn skill(path: &str) -> PathBuf {
    repo().join("skills").join(path)
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("skills-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
        path: repo().join("skills").display().to_string(),
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

fn python(script: &str, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new("python3");
    command
        .arg(skill(script))
        .args(["--yapi", env!("CARGO_BIN_EXE_yapi")])
        .args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

#[test]
fn check_extension_reports_commands_and_load_errors() {
    let dir = scratch("check");
    let template = skill("yapi-extension/templates/pi-extension.ts");
    let output = python(
        "yapi-extension/scripts/check-extension.py",
        &[template.to_str().unwrap()],
        &[("HOME", &dir)],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains("  /hello: Says hello\n"), "{stdout}");

    let broken = dir.join("broken.ts");
    std::fs::write(
        &broken,
        "import missing from \"not-installed\";\nexport default function () { missing(); }\n",
    )
    .unwrap();
    let output = python(
        "yapi-extension/scripts/check-extension.py",
        &[broken.to_str().unwrap()],
        &[("HOME", &dir)],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("not-installed"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_prints_the_answer() {
    let cassette =
        Cassette::load(&repo().join("tests/fixtures/cassettes/anthropic-messages/text.json"))
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
