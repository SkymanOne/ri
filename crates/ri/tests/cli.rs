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
