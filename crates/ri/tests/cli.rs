//! Command-line contract tests for the `ri` binary.

use std::process::Command;

#[test]
fn version_prints_bare_version() {
    for flag in ["--version", "-v"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ri"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag} failed");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

/// `--export` writes the same HTML pi `v1.0.0` does: the SHA-256 of pi's
/// export of each session fixture, made with `pi --export` in a clean
/// environment.
#[test]
fn export_matches_pi_byte_for_byte() {
    use sha2::Digest;
    let fixtures =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pi/sessions");
    let out = std::env::temp_dir().join(format!("ri-export-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    for (name, digest) in [
        (
            "main",
            "abb9b743def853a26bd98116de413fef0cd6b9893e3723f44f5d6a0bd9d07ae7",
        ),
        (
            "branched",
            "20e92450caaa204f7faba57c5a4e2ded18de3c02d2630ed524ac77a564f25248",
        ),
        (
            "legacy-before-compaction",
            "b8142182de87fd3551ee16136670c00dfd53860f47d02ab70ba60146e9d5fdde",
        ),
    ] {
        let target = out.join(format!("{name}.html"));
        let output = Command::new(env!("CARGO_BIN_EXE_ri"))
            .arg("--export")
            .arg(fixtures.join(format!("{name}.jsonl")))
            .arg(&target)
            .env_clear()
            .env("HOME", &out)
            .env("RI_CODING_AGENT_DIR", out.join("agent"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{name}: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("Exported to: {}\n", target.display())
        );
        let html = std::fs::read(&target).unwrap();
        let actual: String = sha2::Sha256::digest(&html)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(actual, digest, "{name}.html differs from pi's export");
    }
    let missing = Command::new(env!("CARGO_BIN_EXE_ri"))
        .args(["--export", "/nonexistent/session.jsonl"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(missing.stderr).unwrap(),
        "Error: File not found: /nonexistent/session.jsonl\n"
    );
    std::fs::remove_dir_all(&out).unwrap();
}

/// Once `-p` or `--mode` owns stdout, help and the model list go to stderr,
/// as in pi; on their own they print to stdout.
#[test]
fn metadata_goes_to_stderr_in_print_and_modes() {
    let home = std::env::temp_dir().join(format!("ri-metadata-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_ri"))
            .args(args)
            .env_clear()
            .env("HOME", &home)
            .env("RI_CODING_AGENT_DIR", home.join("agent"))
            .env("ANTHROPIC_API_KEY", "unused")
            .current_dir(&home)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        (
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };
    let (stdout, stderr) = run(&["--help"]);
    assert!(stdout.starts_with("ri - AI coding assistant"));
    assert!(stderr.is_empty());
    for args in [&["-p", "--help"][..], &["--mode", "json", "--help"]] {
        let (stdout, stderr) = run(args);
        assert!(stdout.is_empty(), "{args:?}");
        assert!(stderr.starts_with("ri - AI coding assistant"), "{args:?}");
    }
    let (stdout, stderr) = run(&["-p", "--list-models", "claude-sonnet-4-5"]);
    assert!(stdout.is_empty());
    assert!(stderr.contains("claude-sonnet-4-5"));
    let (stdout, _) = run(&["--list-models", "claude-sonnet-4-5"]);
    assert!(stdout.contains("claude-sonnet-4-5"));
    let _ = std::fs::remove_dir_all(&home);
}

/// pi's RPC mode starts without any model, reporting pi-agent-core's
/// placeholder, and exits with 143 on SIGTERM even while its input stays
/// open.
#[cfg(unix)]
#[test]
fn rpc_starts_without_models_and_exits_on_sigterm() {
    use std::io::{BufRead, Write};
    let home = std::env::temp_dir().join(format!("ri-rpc-signal-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ri"))
        .args(["--mode", "rpc"])
        .env_clear()
        .env("HOME", &home)
        .env("RI_CODING_AGENT_DIR", home.join("agent"))
        .env("PI_OFFLINE", "1")
        .current_dir(&home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"id":"s","type":"get_state"}}"#).unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(
        line.contains(r#""model":{"id":"unknown","name":"unknown","api":"unknown""#),
        "{line}"
    );
    assert!(line.contains(r#""thinkingLevel":"off""#), "{line}");
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "still running with stdin open"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(143));
    drop(stdin);
    let _ = std::fs::remove_dir_all(&home);
}

/// On SIGTERM, print mode kills the commands its `bash` tool is running, as
/// pi does, instead of leaving them behind.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn print_mode_kills_running_commands_on_sigterm() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let cassette =
        ri_mock::Cassette::load(&fixtures.join("cassettes/anthropic-messages/sleep-tool.json"))
            .unwrap();
    let server = ri_mock::MockServer::start("127.0.0.1:0".parse().unwrap(), cassette)
        .await
        .unwrap();
    let home = std::env::temp_dir().join(format!("ri-print-signal-{}", std::process::id()));
    std::fs::create_dir_all(home.join("agent")).unwrap();
    std::fs::write(
        home.join("agent/models.json"),
        format!(
            r#"{{"providers":{{"anthropic":{{"baseUrl":"{}"}}}}}}"#,
            server.url()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ri"))
        .args([
            "-p",
            "--no-session",
            "--model",
            "anthropic/claude-sonnet-4-5",
            "go",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("RI_CODING_AGENT_DIR", home.join("agent"))
        .env("ANTHROPIC_API_KEY", "mock")
        .env("PI_OFFLINE", "1")
        .current_dir(&home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let running = || {
        Command::new("pgrep")
            .args(["-f", r"^sleep 31\.7$"])
            .output()
            .is_ok_and(|output| !output.stdout.is_empty())
    };
    let started = std::time::Instant::now();
    while !running() {
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let status = child.wait().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let left = running();
    if left {
        let _ = Command::new("pkill")
            .args(["-f", r"^sleep 31\.7$"])
            .status();
    }
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(status.code(), Some(143));
    assert!(!left, "the command outlived ri");
}

/// `list` tags each installed package by its extensions: `[npm]` for pi
/// extensions, `[wasm]` for native ones, both for a package with each, and
/// nothing for a package without extensions or one that is not installed.
#[test]
fn list_tags_extension_kinds() {
    let root = std::env::temp_dir().join(format!("ri-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let files = [
        "js/index.ts",
        "native/extensions/shout.wasm",
        "mixed/extensions/a.ts",
        "mixed/extensions/b.wasm",
        "skills/skills/demo/SKILL.md",
        "filtered/index.ts",
        "single.wasm",
    ];
    for file in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
    }
    let agent = root.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let path = |name: &str| root.join(name).display().to_string();
    let settings = serde_json::json!({"packages": [
        path("js"),
        path("native"),
        path("mixed"),
        path("skills"),
        {"source": path("filtered"), "extensions": []},
        path("single.wasm"),
        "npm:not-installed",
    ]});
    std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ri"))
        .arg("list")
        .current_dir(&root)
        .env_clear()
        .env("HOME", &root)
        .env("RI_CODING_AGENT_DIR", &agent)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let package = |name: &str, tag: &str| format!("  {}{tag}\n    {}\n", path(name), path(name));
    let expected = [
        "User packages:\n".to_owned(),
        package("js", " [npm]"),
        package("native", " [wasm]"),
        package("mixed", " [npm, wasm]"),
        package("skills", ""),
        format!(
            "  {} (filtered)\n    {}\n",
            path("filtered"),
            path("filtered")
        ),
        package("single.wasm", " [wasm]"),
        "  npm:not-installed\n".to_owned(),
    ]
    .concat();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    let _ = std::fs::remove_dir_all(&root);
}
