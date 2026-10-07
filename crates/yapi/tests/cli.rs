//! Command-line contract tests for the `yapi` binary.

mod common;

use std::process::Command;

#[test]
fn version_prints_bare_version() {
    for flag in ["--version", "-v"] {
        let output = Command::new(env!("CARGO_BIN_EXE_yapi"))
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
    let fixtures = common::fixtures().join("pi/sessions");
    let out = common::scratch("export");
    for (name, digest) in [
        (
            "main",
            "fccaf043e37ff3994301e1823521b6d23b529e809776c2db498c3ef5f83bbe37",
        ),
        (
            "branched",
            "8310b9e08a3749c9d1e5f34ada5d60b269ddeba676cd4d94dbda762ee5aebb7a",
        ),
        (
            "legacy-before-compaction",
            "b8142182de87fd3551ee16136670c00dfd53860f47d02ab70ba60146e9d5fdde",
        ),
    ] {
        let target = out.join(format!("{name}.html"));
        let output = common::yapi(&out)
            .arg("--export")
            .arg(fixtures.join(format!("{name}.jsonl")))
            .arg(&target)
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
    let missing = Command::new(env!("CARGO_BIN_EXE_yapi"))
        .args(["--export", "/nonexistent/session.jsonl"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(missing.stderr).unwrap(),
        "Error: File not found: /nonexistent/session.jsonl\n"
    );
}

/// Once `-p` or `--mode` owns stdout, help and the model list go to stderr,
/// as in pi; on their own they print to stdout.
#[test]
fn metadata_goes_to_stderr_in_print_and_modes() {
    let home = common::scratch("metadata");
    let run = |args: &[&str]| {
        let output = common::yapi(&home)
            .args(args)
            .env("ANTHROPIC_API_KEY", "unused")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        (
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };
    let (stdout, stderr) = run(&["--help"]);
    assert!(stdout.starts_with("yapi - AI coding assistant"));
    assert!(stderr.is_empty());
    for args in [&["-p", "--help"][..], &["--mode", "json", "--help"]] {
        let (stdout, stderr) = run(args);
        assert!(stdout.is_empty(), "{args:?}");
        assert!(stderr.starts_with("yapi - AI coding assistant"), "{args:?}");
    }
    let (stdout, stderr) = run(&["-p", "--list-models", "claude-sonnet-4-5"]);
    assert!(stdout.is_empty());
    assert!(stderr.contains("claude-sonnet-4-5"));
    let (stdout, _) = run(&["--list-models", "claude-sonnet-4-5"]);
    assert!(stdout.contains("claude-sonnet-4-5"));
}

/// pi's RPC mode starts without any model, reporting pi-agent-core's
/// placeholder, and exits with 143 on SIGTERM even while its input stays
/// open.
#[cfg(unix)]
#[test]
fn rpc_starts_without_models_and_exits_on_sigterm() {
    use std::io::{BufRead, Write};
    let home = common::scratch("rpc-signal");
    let mut child = common::yapi(&home)
        .args(["--mode", "rpc"])
        .env("PI_OFFLINE", "1")
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
    let cassette = yapi_mock::Cassette::load(
        &common::fixtures().join("cassettes/anthropic-messages/sleep-tool.json"),
    )
    .unwrap();
    let server = yapi_mock::MockServer::start("127.0.0.1:0".parse().unwrap(), cassette)
        .await
        .unwrap();
    let home = common::scratch("print-signal");
    std::fs::create_dir_all(home.join("agent")).unwrap();
    std::fs::write(
        home.join("agent/models.json"),
        format!(
            r#"{{"providers":{{"anthropic":{{"baseUrl":"{}"}}}}}}"#,
            server.url()
        ),
    )
    .unwrap();
    let mut child = common::yapi(&home)
        .args([
            "-p",
            "--no-session",
            "--model",
            "anthropic/claude-sonnet-4-5",
            "go",
        ])
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("PI_OFFLINE", "1")
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
    assert!(!left, "the command outlived yapi");
}

/// `list` tags each installed package by its extensions: `[npm]` for pi
/// extensions, `[wasm]` for native ones, both for a package with each, and
/// nothing for a package without extensions or one that is not installed.
#[test]
fn list_tags_extension_kinds() {
    let root = common::scratch("list");
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
    let output = common::yapi(&root).arg("list").output().unwrap();
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
}

/// pi's service and easter egg commands report that yapi does not offer them,
/// without suggesting that a later version will.
#[test]
fn service_commands_are_not_offered() {
    use std::time::Duration;
    let root = common::scratch("unavailable");
    let agent = root.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let env = [
        ("HOME", root.clone().into_os_string()),
        ("YAPI_CODING_AGENT_DIR", agent.into_os_string()),
        ("PI_OFFLINE", "1".into()),
        ("ANTHROPIC_API_KEY", "mock".into()),
    ];
    let args = ["--model", "anthropic/claude-sonnet-4-5", "--no-session"].map(str::to_owned);
    let mut pty = yapi_mock::pty::Pty::spawn(
        std::path::Path::new(env!("CARGO_BIN_EXE_yapi")),
        &args,
        &root,
        &env,
        (100, 30),
        true,
    )
    .unwrap();
    let painted = |rows: &[String]| rows.iter().any(|row| row.contains("claude-sonnet-4-5"));
    assert!(pty.wait_for(Duration::from_secs(20), painted).is_some());
    pty.write("/share\r").unwrap();
    let reported = |rows: &[String]| {
        rows.iter()
            .any(|row| row.trim_end().ends_with("/share is not available in yapi"))
    };
    assert!(
        pty.wait_for(Duration::from_secs(10), reported).is_some(),
        "{:#?}",
        pty.rows()
    );
    pty.finish().unwrap();
}

#[test]
fn update_self_names_the_installer() {
    let dir = common::scratch("update-self");
    // pi's `--force` reinstalls pi; yapi accepts it for `update` only.
    for args in [&["update", "self"][..], &["update", "--force"]] {
        let output = common::yapi(&dir)
            .args(args)
            .env("PI_OFFLINE", "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "yapi cannot update itself. Install the latest release the way you installed yapi, such as:\n  curl -fsSL https://raw.githubusercontent.com/SkymanOne/yapi/main/install.sh | sh\n"
        );
    }
    let output = common::yapi(&dir)
        .args(["install", "./pkg", "--force"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("Unknown option --force for \"install\".\n")
    );
}

/// `yapi mcp login` signs in through the browser, here a script that follows
/// the authorization redirect to the callback, against the OAuth test server
/// in `tests/fixtures/mcp`. Needs `python3` on `PATH`.
#[cfg(unix)]
#[test]
fn mcp_login_and_logout() {
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;

    let home = common::scratch("mcp-login");
    let mut server = Command::new("python3")
        .arg(common::fixtures().join("mcp/server.py"))
        .args(["--http", "--oauth"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut url = String::new();
    std::io::BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut url)
        .unwrap();
    let url = url.trim().to_owned();
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["open", "xdg-open"] {
        let script = bin.join(name);
        std::fs::write(
            &script,
            "#!/bin/sh\nexec python3 -c 'import sys, urllib.request; urllib.request.urlopen(sys.argv[1]).read()' \"$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::create_dir_all(home.join("agent")).unwrap();
    std::fs::write(
        home.join("agent/mcp.json"),
        format!("{{\"mcpServers\": {{\"demo\": {{\"url\": \"{url}\"}}}}}}\n"),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        let output = common::yapi(&home)
            .env("PATH", &path)
            .args(args)
            .output()
            .unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };

    let (code, stdout, _) = run(&["mcp", "list"]);
    assert_eq!(code, Some(1));
    assert_eq!(
        stdout,
        format!(
            "demo: needs sign-in (codemode, global)\n  {url}\n  sign in with: yapi mcp login demo\n"
        )
    );
    let (code, stdout, stderr) = run(&["mcp", "login", "demo", "--timeout", "30"]);
    assert_eq!(code, Some(0), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "Sign in to MCP server \"demo\" in your browser:");
    assert!(
        lines[1]
            .starts_with(&url.replace("/mcp", "/authorize?response_type=code&client_id=client-1&"))
    );
    assert_eq!(lines[2..], ["Signed in to MCP server \"demo\" (9 tools)."]);
    let (code, stdout, _) = run(&["mcp", "login", "demo"]);
    assert_eq!(code, Some(0));
    assert_eq!(
        stdout,
        "Already signed in to MCP server \"demo\" (9 tools).\n"
    );
    let (code, stdout, _) = run(&["mcp", "list"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("demo: connected, 9 tools (codemode, global)\n"));
    assert_eq!(
        run(&["mcp", "logout", "demo"]),
        (
            Some(0),
            "Signed out of MCP server \"demo\".\n".into(),
            String::new()
        )
    );
    assert_eq!(
        run(&["mcp", "logout", "demo"]),
        (
            Some(0),
            "No stored credentials for MCP server \"demo\".\n".into(),
            String::new()
        )
    );
    assert_eq!(
        run(&["mcp", "login", "demo", "--timeout", "0"]).2,
        "--timeout must be a positive number of seconds.\n"
    );
    server.kill().unwrap();
    server.wait().unwrap();
}
