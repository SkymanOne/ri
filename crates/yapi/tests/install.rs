//! `install.sh` against releases packaged by `scripts/package-release.sh`,
//! served the way GitHub serves them. The installed "binary" is a script that
//! prints its version.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("install-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Packages a fake `yapi` that prints `version` for every target, under
/// `<site>/releases/download/v<version>/`.
fn release(site: &Path, version: &str) {
    std::fs::create_dir_all(site).unwrap();
    let binary = site.join(format!("yapi-{version}"));
    std::fs::write(&binary, format!("#!/bin/sh\necho {version}\n")).unwrap();
    let out = site.join(format!("releases/download/v{version}"));
    for target in TARGETS {
        let output = Command::new("sh")
            .arg(repo().join("scripts/package-release.sh"))
            .arg(&binary)
            .arg(target)
            .arg(&out)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
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
        .arg(repo().join("install.sh"))
        .args(args)
        .env("YAPI_RELEASES_URL", releases)
        .env("HOME", home)
        .env_remove("YAPI_VERSION")
        .env_remove("YAPI_INSTALL_DIR")
        .output()
        .unwrap()
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
        .arg(repo().join("install.sh"))
        .env("YAPI_RELEASES_URL", &releases)
        .env("YAPI_VERSION", "v9.9.9")
        .env("YAPI_INSTALL_DIR", &to)
        .env("HOME", &dir)
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

/// Each archive holds only the binary, so `curl ... | tar xzf -` into a
/// directory on PATH installs it, as the install guide shows.
#[test]
fn archives_unpack_to_just_the_binary() {
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
            "yapi\n",
            "{target}"
        );
    }
    let releases = serve(site, Some("9.9.9"));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "curl -fsSL {releases}/latest/download/yapi-{}.tar.gz | tar xzf - -C {}",
            TARGETS[0],
            bin.display()
        ))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(installed_version(&bin.join("yapi")), "9.9.9");
}
