//! Packages against a mock npm registry: installs with hoisted dependencies,
//! settings entries, resources, removal, native addons and integrity.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use serde_json::{Value, json};
use sha2::Digest as _;
use yapi_core::packages::PackageManager;
use yapi_core::settings::{Scope, SettingsManager};
use yapi_mock::MockServer;
use yapi_mock::{Cassette, Interaction, RequestMatch, Response};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yapi-packages-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An npm tarball with `files` under `package/`.
fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, text) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("package/{name}"), text.as_bytes())
            .unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn json_response(body: &Value) -> Response {
    let mut headers = IndexMap::new();
    headers.insert("content-type".into(), "application/json".into());
    Response {
        status: 200,
        headers,
        chunks: vec![body.to_string()],
        body_base64: None,
        chunk_delay_ms: 0,
    }
}

fn get(path: &str, response: Response) -> Interaction {
    Interaction {
        request: RequestMatch {
            method: "GET".into(),
            path: path.into(),
        },
        response,
    }
}

/// The packument and tarball interactions for `name@version`.
fn publish(
    base: &str,
    name: &str,
    versions: &[(&str, Value, Vec<u8>)],
    served: &str,
) -> Vec<Interaction> {
    let mut manifests = serde_json::Map::new();
    let mut tarball_bytes = Vec::new();
    for (version, manifest, bytes) in versions {
        let mut manifest = manifest.clone();
        manifest["name"] = json!(name);
        manifest["version"] = json!(version);
        manifest["dist"] = json!({
            "tarball": format!("{base}/{name}/-/{name}-{version}.tgz"),
            "integrity": format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(bytes))),
        });
        manifests.insert((*version).to_owned(), manifest);
        if *version == served {
            tarball_bytes = bytes.clone();
        }
    }
    let latest = versions.last().unwrap().0;
    let packument = json!({"name": name, "dist-tags": {"latest": latest}, "versions": manifests});
    vec![
        get(&format!("/{name}"), json_response(&packument)),
        get(
            &format!("/{name}/-/{name}-{served}.tgz"),
            Response {
                status: 200,
                headers: IndexMap::new(),
                chunks: Vec::new(),
                body_base64: Some(STANDARD.encode(tarball_bytes)),
                chunk_delay_ms: 0,
            },
        ),
    ]
}

async fn registry(interactions: impl Fn(&str) -> Vec<Interaction>) -> MockServer {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    MockServer::start(
        format!("127.0.0.1:{port}").parse().unwrap(),
        Cassette {
            interactions: interactions(&base),
        },
    )
    .await
    .unwrap()
}

fn manager(dir: &Path, url: &str) -> PackageManager {
    let cwd = dir.join("project");
    let agent = dir.join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    let settings = SettingsManager::load(&agent, &cwd, false);
    PackageManager::new(cwd, agent, settings, format!("{url}/"))
}

#[tokio::test(flavor = "multi_thread")]
async fn installs_npm_packages_with_dependencies() {
    let dir = scratch("npm");
    let extension = tarball(&[
        (
            "package.json",
            r#"{"name": "ext-pkg", "pi": {"extensions": ["./index.ts"]}}"#,
        ),
        ("index.ts", "export default function () {}\n"),
    ]);
    let pad = |version: &str| {
        tarball(&[
            (
                "package.json",
                &format!(r#"{{"name": "left-pad", "version": "{version}"}}"#),
            ),
            ("index.js", "module.exports = 1;\n"),
        ])
    };
    let server = registry(|base| {
        let mut all = publish(
            base,
            "ext-pkg",
            &[(
                "1.0.0",
                json!({"dependencies": {"left-pad": "^1.1.0", "@earendil-works/pi-coding-agent": "*"}}),
                extension.clone(),
            )],
            "1.0.0",
        );
        all.extend(publish(
            base,
            "left-pad",
            &[("1.0.0", json!({}), pad("1.0.0")), ("1.3.0", json!({}), pad("1.3.0")), ("2.0.0", json!({}), pad("2.0.0"))],
            "1.3.0",
        ));
        all
    })
    .await;
    let mut packages = manager(&dir, &server.url());
    let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = progress.clone();
    packages.on_progress(move |line| log.lock().unwrap().push(line.to_owned()));
    packages.install("npm:ext-pkg", false).await.unwrap();
    assert_eq!(*progress.lock().unwrap(), ["Installing npm:ext-pkg..."]);

    let root = dir.join("agent/npm");
    assert!(root.join("node_modules/ext-pkg/index.ts").exists());
    let pad_manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("node_modules/left-pad/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pad_manifest["version"], "1.3.0");
    assert!(!root.join("node_modules/@earendil-works").exists());
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        "*\n!.gitignore\n"
    );
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        manifest,
        json!({"name": "pi-extensions", "private": true, "dependencies": {"ext-pkg": "^1.0.0"}})
    );
    assert_eq!(
        packages.settings().document(Scope::Global)["packages"],
        json!(["npm:ext-pkg"])
    );

    let listed = packages.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].installed_path.as_deref(),
        Some(root.join("node_modules/ext-pkg").as_path())
    );
    assert_eq!(
        listed[0].extensions,
        [root.join("node_modules/ext-pkg/index.ts")]
    );

    assert!(packages.remove("npm:ext-pkg", false).await.unwrap());
    assert!(!root.join("node_modules/ext-pkg").exists());
    assert!(
        !root.join("node_modules/left-pad").exists(),
        "unreferenced dependencies are pruned"
    );
    assert_eq!(
        packages.settings().document(Scope::Global)["packages"],
        json!([])
    );
    assert!(!packages.remove("npm:ext-pkg", false).await.unwrap());
    server.finish().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn installs_native_addons_unbuilt_and_rejects_bad_integrity() {
    let dir = scratch("native");
    let native = tarball(&[
        (
            "package.json",
            r#"{"name": "native-pkg", "scripts": {"install": "node-gyp rebuild"}}"#,
        ),
        ("binding.gyp", "{}"),
    ]);
    let server = registry(|base| {
        publish(
            base,
            "native-pkg",
            &[("1.0.0", json!({}), native.clone())],
            "1.0.0",
        )
    })
    .await;
    let mut packages = manager(&dir, &server.url());
    packages
        .install("npm:native-pkg@1.0.0", false)
        .await
        .unwrap();
    let installed = dir.join("agent/npm/node_modules/native-pkg");
    assert!(installed.join("binding.gyp").exists());
    // No install script ran, so nothing was built.
    assert!(!installed.join("build").exists());

    let dir = scratch("integrity");
    let good = tarball(&[("package.json", r#"{"name": "pkg"}"#)]);
    let server = registry(|base| {
        let mut all = publish(base, "pkg", &[("1.0.0", json!({}), good.clone())], "1.0.0");
        all[1].response.body_base64 = Some(STANDARD.encode(b"tampered"));
        all
    })
    .await;
    let mut packages = manager(&dir, &server.url());
    let error = packages.install("npm:pkg", false).await.unwrap_err();
    assert_eq!(error.to_string(), "Integrity check failed for pkg@1.0.0");
}

/// An `npm:` alias installs the package it names under the alias, and
/// packuments are asked for in npm's abbreviated form.
#[tokio::test(flavor = "multi_thread")]
async fn installs_aliased_dependencies_from_abbreviated_packuments() {
    let dir = scratch("alias");
    let app = tarball(&[("package.json", r#"{"name": "app-pkg"}"#)]);
    let fork = tarball(&[(
        "package.json",
        r#"{"name": "@forks/pty", "version": "0.13.2"}"#,
    )]);
    let server = registry(|base| {
        let mut all = publish(
            base,
            "app-pkg",
            &[(
                "1.0.0",
                json!({"dependencies": {"node-pty": "npm:@forks/pty@^0.13.1"}}),
                app.clone(),
            )],
            "1.0.0",
        );
        let mut forked = publish(
            base,
            "@forks/pty",
            &[
                ("0.13.2", json!({}), fork.clone()),
                ("1.0.0", json!({}), fork.clone()),
            ],
            "0.13.2",
        );
        forked[0].request.path = "/@forks%2fpty".into();
        all.extend(forked);
        all
    })
    .await;
    let mut packages = manager(&dir, &server.url());
    packages.install("npm:app-pkg", false).await.unwrap();

    let modules = dir.join("agent/npm/node_modules");
    let installed: Value = serde_json::from_str(
        &std::fs::read_to_string(modules.join("node-pty/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(installed["name"], "@forks/pty");
    assert_eq!(installed["version"], "0.13.2");
    assert!(!modules.join("@forks").exists());
    let requests = server.finish().unwrap();
    let packuments: Vec<_> = requests
        .iter()
        .filter(|request| !request.path.ends_with(".tgz"))
        .collect();
    assert_eq!(packuments.len(), 2);
    for request in packuments {
        assert!(
            request.headers["accept"].starts_with("application/vnd.npm.install-v1+json"),
            "{request:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn local_packages_are_stored_relative_to_the_scope() {
    let dir = scratch("local");
    let mut packages = manager(&dir, "http://127.0.0.1:9");
    std::fs::create_dir_all(dir.join("project/tools")).unwrap();
    std::fs::write(
        dir.join("project/tools/index.ts"),
        "export default () => {};\n",
    )
    .unwrap();
    let previous = std::env::current_dir().unwrap();
    packages
        .install(&dir.join("project/tools").to_string_lossy(), false)
        .await
        .unwrap();
    assert_eq!(
        packages.settings().document(Scope::Global)["packages"],
        json!(["../project/tools"])
    );
    let error = packages.install("./missing", false).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "Path does not exist: {}",
            dir.join("project/missing").display()
        )
    );
    assert_eq!(
        packages.list()[0].extensions,
        [dir.join("project/tools/index.ts")]
    );
    assert_eq!(std::env::current_dir().unwrap(), previous);
}

/// Native extensions install like any package: a built `.wasm` file by path,
/// a folder with an `extensions` directory, or an npm package whose `yapi` key
/// names its `.wasm` build ahead of the `pi` key's JavaScript.
#[tokio::test(flavor = "multi_thread")]
async fn installs_native_extensions() {
    const WASM: &str = "\0asm\u{1}\0\0\0";
    let dir = scratch("native-extensions");
    let shout = tarball(&[
        (
            "package.json",
            r#"{"name": "shout", "pi": {"extensions": ["./dist/shout.js"]}, "yapi": {"extensions": ["./dist/shout.wasm"]}}"#,
        ),
        ("dist/shout.js", "export default function () {}\n"),
        ("dist/shout.wasm", WASM),
    ]);
    let server = registry(|base| {
        publish(
            base,
            "shout",
            &[("1.0.0", json!({}), shout.clone())],
            "1.0.0",
        )
    })
    .await;
    let mut packages = manager(&dir, &server.url());

    let file = dir.join("project/target/shout.wasm");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, WASM).unwrap();
    packages
        .install(&file.to_string_lossy(), false)
        .await
        .unwrap();

    let folder = dir.join("project/shout-package");
    std::fs::create_dir_all(folder.join("extensions")).unwrap();
    std::fs::write(folder.join("extensions/shout.wasm"), WASM).unwrap();
    packages
        .install(&folder.to_string_lossy(), false)
        .await
        .unwrap();

    packages.install("npm:shout", false).await.unwrap();
    let installed = dir.join("agent/npm/node_modules/shout/dist/shout.wasm");
    assert_eq!(std::fs::read(&installed).unwrap(), WASM.as_bytes());

    let extensions: Vec<PathBuf> = packages
        .list()
        .into_iter()
        .flat_map(|package| package.extensions)
        .collect();
    assert_eq!(
        extensions,
        [file, folder.join("extensions/shout.wasm"), installed]
    );
    server.finish().unwrap();
}

/// `-e npm:` installs into pi's temporary folder for one run without
/// touching settings, reuses an install whose version is in range, and
/// offline installs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn installs_temporary_packages_outside_settings() {
    let dir = scratch("temporary");
    let package = tarball(&[
        (
            "package.json",
            r#"{"name": "temp-pkg", "version": "1.2.0"}"#,
        ),
        ("extensions/index.ts", "export default function () {}\n"),
    ]);
    let server = registry(|base| {
        publish(
            base,
            "temp-pkg",
            &[("1.2.0", json!({}), package.clone())],
            "1.2.0",
        )
    })
    .await;
    let mut packages = manager(&dir, &server.url());

    let offline = packages.install_temporary("npm:temp-pkg", true).await;
    assert_eq!(offline.unwrap(), None);
    let git = packages
        .install_temporary("git:github.com/user/repo", true)
        .await;
    assert_eq!(git.unwrap(), None);

    // pi's `getTemporaryDir("npm")`: the hash is SHA-256 of `npm-`.
    let installed = dir.join("agent/tmp/extensions/npm/f35b2129/node_modules/temp-pkg");
    let path = packages.install_temporary("npm:temp-pkg", false).await;
    assert_eq!(path.unwrap().as_deref(), Some(installed.as_path()));
    assert!(installed.join("extensions/index.ts").exists());
    for (source, offline) in [
        ("npm:temp-pkg@^1.1", false),
        ("npm:temp-pkg@latest", false),
        ("npm:temp-pkg", true),
    ] {
        let path = packages.install_temporary(source, offline).await;
        assert_eq!(
            path.unwrap().as_deref(),
            Some(installed.as_path()),
            "{source}"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.join("agent/tmp/extensions"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    assert!(packages.list().is_empty());
    assert!(!dir.join("agent/settings.json").exists());
    assert_eq!(
        server.finish().unwrap().len(),
        2,
        "one packument and one tarball"
    );
}
