//! `install.sh` against releases packaged by `scripts/package-release.sh`,
//! served the way GitHub serves them. The installed "binary" is a script that
//! prints its version, and the docs archive holds `.version` and `index.md`.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::Digest as _;

const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

fn scratch(name: &str) -> PathBuf {
    common::scratch(&format!("install-{name}"))
}

/// Packages a fake `yapi` that prints `version` for every target, with fake
/// third-party notices, and a docs archive naming `version`, under
/// `<site>/releases/download/v<version>/`.
fn release(site: &Path, version: &str) {
    std::fs::create_dir_all(site).unwrap();
    let binary = site.join(format!("yapi-{version}"));
    std::fs::write(&binary, format!("#!/bin/sh\necho {version}\n")).unwrap();
    let notices = site.join("THIRD-PARTY-NOTICES");
    std::fs::write(&notices, "notices\n").unwrap();
    let out = site.join(format!("releases/download/v{version}"));
    for target in TARGETS {
        let output = Command::new("sh")
            .arg(common::repo().join("scripts/package-release.sh"))
            .arg(&binary)
            .arg(target)
            .arg(&out)
            .arg(&notices)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let docs = site.join(format!("docs-{version}"));
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join(".version"), format!("{version}\n")).unwrap();
    std::fs::write(docs.join("index.md"), format!("# yapi {version}\n")).unwrap();
    let archive = out.join("yapi-docs.tar.gz");
    let output = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&docs)
        .args([".version", "index.md"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let digest = sha2::Sha256::digest(std::fs::read(&archive).unwrap());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    std::fs::write(
        out.join("yapi-docs.tar.gz.sha256"),
        format!("{hex}  yapi-docs.tar.gz\n"),
    )
    .unwrap();
}

/// Serves `site` over HTTP until the process exits, redirecting the latest
/// release's downloads to release `latest` as GitHub does, or answering them
/// with 404 when there is no release.
fn serve(site: PathBuf, latest: Option<&'static str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let host = base.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            let path = request.split(' ').nth(1).unwrap_or("/").to_owned();
            let latest_file = path.strip_prefix("/releases/latest/download/");
            let (status, headers, body) = match (latest_file, latest) {
                (Some(file), Some(tag)) => (
                    "302 Found",
                    format!("Location: {host}/releases/download/v{tag}/{file}\r\n"),
                    Vec::new(),
                ),
                _ => match std::fs::read(site.join(path.trim_start_matches('/'))) {
                    Ok(body) => ("200 OK", String::new(), body),
                    Err(_) => ("404 Not Found", String::new(), Vec::new()),
                },
            };
            let head = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    format!("{base}/releases")
}

fn install(releases: &str, home: &Path, args: &[&str]) -> Output {
    Command::new("sh")
        .arg(common::repo().join("install.sh"))
        .args(args)
        .env("YAPI_RELEASES_URL", releases)
        .env("HOME", home)
        .env_remove("YAPI_VERSION")
        .env_remove("YAPI_INSTALL_DIR")
        .env_remove("YAPI_NO_DOCS")
        .env_remove("YAPI_CODING_AGENT_DIR")
        .output()
        .unwrap()
}

/// The `index.md` of the docs installed in `agent`, if any.
fn docs_index(agent: &Path) -> Option<String> {
    std::fs::read_to_string(agent.join("docs/index.md")).ok()
}

fn installed_version(binary: &Path) -> String {
    let output = Command::new(binary).output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn installs_the_latest_release_into_local_bin() {
    let dir = scratch("latest");
    let site = dir.join("site");
    release(&site, "9.9.8");
    release(&site, "9.9.9");
    let output = install(&serve(site, Some("9.9.9")), &dir, &[]);
    assert!(output.status.success(), "{output:?}");
    let binary = dir.join(".local/bin/yapi");
    assert_eq!(installed_version(&binary), "9.9.9");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("Installed yapi 9.9.9 to {}", binary.display())));
    // The release's docs for the model, in the default agent directory.
    let agent = dir.join(".yapi/agent");
    assert_eq!(docs_index(&agent).as_deref(), Some("# yapi 9.9.9\n"));
    assert!(stdout.contains(&format!(
        "Installed the docs for the model to {}",
        agent.join("docs").display()
    )));
}

#[test]
fn replaces_the_docs_unless_asked_not_to() {
    let dir = scratch("docs");
    let site = dir.join("site");
    release(&site, "9.9.9");
    let releases = serve(site, Some("9.9.9"));
    let agent = dir.join("agent");
    std::fs::create_dir_all(agent.join("docs")).unwrap();
    std::fs::write(agent.join("docs/stale.md"), "old").unwrap();
    let with_agent = |args: &[&str], no_docs: Option<&str>| {
        let mut command = Command::new("sh");
        command
            .arg(common::repo().join("install.sh"))
            .args(args)
            .env("YAPI_RELEASES_URL", &releases)
            .env("HOME", &dir)
            .env("YAPI_CODING_AGENT_DIR", &agent)
            .env_remove("YAPI_VERSION")
            .env_remove("YAPI_INSTALL_DIR")
            .env_remove("YAPI_NO_DOCS");
        if let Some(value) = no_docs {
            command.env("YAPI_NO_DOCS", value);
        }
        command.output().unwrap()
    };
    let output = with_agent(&[], None);
    assert!(output.status.success(), "{output:?}");
    // The new copy replaces the old one whole, and nothing is left beside it.
    assert_eq!(docs_index(&agent).as_deref(), Some("# yapi 9.9.9\n"));
    assert!(!agent.join("docs/stale.md").exists());
    let names: Vec<_> = std::fs::read_dir(&agent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["docs"]);

    // `--no-docs` and `YAPI_NO_DOCS` keep the docs as they are.
    std::fs::write(agent.join("docs/index.md"), "edited").unwrap();
    for (args, no_docs) in [(&["--no-docs"][..], None), (&[][..], Some("1"))] {
        let output = with_agent(args, no_docs);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(docs_index(&agent).as_deref(), Some("edited"));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains("Installed the docs"), "{stdout}");
    }
}

#[test]
fn installs_yapi_without_docs_that_fail_their_checksum() {
    let dir = scratch("docs-checksum");
    let site = dir.join("site");
    release(&site, "9.9.9");
    let sums = site.join("releases/download/v9.9.9/yapi-docs.tar.gz.sha256");
    let text = std::fs::read_to_string(&sums).unwrap();
    std::fs::write(&sums, format!("{}{}", "0".repeat(64), &text[64..])).unwrap();
    let output = install(&serve(site, Some("9.9.9")), &dir, &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(installed_version(&dir.join(".local/bin/yapi")), "9.9.9");
    assert_eq!(docs_index(&dir.join(".yapi/agent")), None);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("skipped the docs for the model: yapi-docs.tar.gz does not match its SHA-256 checksum. yapi downloads them when it first runs."),
        "{stderr}"
    );
}

#[test]
fn installs_a_chosen_version_where_asked() {
    let dir = scratch("pinned");
    let site = dir.join("site");
    release(&site, "9.9.8");
    release(&site, "9.9.9");
    let releases = serve(site, Some("9.9.9"));
    let to = dir.join("bin");
    let output = install(
        &releases,
        &dir,
        &["--version", "9.9.8", "--to", to.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(installed_version(&to.join("yapi")), "9.9.8");

    // The environment variables do the same, and a newer install replaces it.
    let output = Command::new("sh")
        .arg(common::repo().join("install.sh"))
        .env("YAPI_RELEASES_URL", &releases)
        .env("YAPI_VERSION", "v9.9.9")
        .env("YAPI_INSTALL_DIR", &to)
        .env("HOME", &dir)
        .env_remove("YAPI_CODING_AGENT_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(installed_version(&to.join("yapi")), "9.9.9");
    assert_eq!(std::fs::read_dir(&to).unwrap().count(), 1);
}

#[test]
fn refuses_a_download_that_fails_its_checksum() {
    let dir = scratch("checksum");
    let site = dir.join("site");
    release(&site, "9.9.9");
    let download = site.join("releases/download/v9.9.9");
    for entry in std::fs::read_dir(&download).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "sha256")
        {
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&path, format!("{}{}", "0".repeat(64), &text[64..])).unwrap();
        }
    }
    let output = install(&serve(site, Some("9.9.9")), &dir, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("does not match its SHA-256 checksum"),
        "{stderr}"
    );
    assert!(!dir.join(".local/bin/yapi").exists());
}

#[test]
fn points_to_a_source_build_without_a_release() {
    let dir = scratch("none");
    let output = install(&serve(dir.join("site"), None), &dir, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("in the latest release at"), "{stderr}");
    assert!(
        stderr.contains("cargo install --locked --git https://github.com/SkymanOne/yapi yapi"),
        "{stderr}"
    );
}

/// Each archive holds the binary and its license files, and extracting
/// `yapi` from it into a directory on PATH installs only the binary, as the
/// install guide shows.
#[test]
fn archives_hold_the_binary_and_its_licenses() {
    let dir = scratch("archive");
    let site = dir.join("site");
    release(&site, "9.9.9");
    let download = site.join("releases/download/v9.9.9");
    for target in TARGETS {
        let listing = Command::new("tar")
            .arg("-tzf")
            .arg(download.join(format!("yapi-{target}.tar.gz")))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(listing.stdout).unwrap(),
            "yapi\nLICENSE-MIT\nLICENSE-APACHE\nTHIRD-PARTY-NOTICES\n",
            "{target}"
        );
    }
    let releases = serve(site, Some("9.9.9"));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "curl -fsSL {releases}/latest/download/yapi-{}.tar.gz | tar xzf - -C {} yapi",
            TARGETS[0],
            bin.display()
        ))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(installed_version(&bin.join("yapi")), "9.9.9");
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1, "only yapi");
}
