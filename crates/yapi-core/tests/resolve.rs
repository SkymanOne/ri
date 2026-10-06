//! yapi's resource resolver against `tests/fixtures/pi/resolve/cases.json`,
//! recorded from pi's `DefaultPackageManager.resolve` by
//! `tests/fixtures/pi/generator/resolve.mjs`.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use yapi_core::packages::resolve::{
    PackageInput, ResolveInput, ResolvedPaths, ResourceType, package_inputs, package_resources,
    resolve, settings_lists,
};
use yapi_core::packages::source::{Source, local_path};

fn fixtures() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pi/resolve/cases.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn document(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn as_json(resolved: &ResolvedPaths, root: &Path) -> Value {
    let root = root.to_string_lossy().into_owned();
    let relative = |text: &str| text.replace(&root, "<root>");
    let mut out = Map::new();
    for kind in ResourceType::ALL {
        let list: Vec<Value> = resolved
            .of(kind)
            .iter()
            .map(|resource| {
                let info = &resource.info;
                let mut entry = json!({
                    "path": relative(&info.path),
                    "enabled": resource.enabled,
                    "source": relative(&info.source),
                    "scope": info.scope,
                    "origin": info.origin,
                });
                if let Some(base) = &info.base_dir {
                    entry["baseDir"] = json!(relative(base));
                }
                entry
            })
            .collect();
        out.insert(kind.key().to_owned(), Value::Array(list));
    }
    Value::Object(out)
}

#[test]
fn resolves_resources_as_pi() {
    let scratch = std::env::temp_dir().join(format!("yapi-resolve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    for (name, case) in fixtures().as_object().unwrap() {
        let root = scratch.join(name);
        for (file, text) in case["files"].as_object().unwrap() {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text.as_str().unwrap()).unwrap();
        }
        for dir in ["agent", "project", "home"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = std::fs::canonicalize(&root).unwrap();
        let trusted = case["trusted"].as_bool().unwrap();
        let agent_dir = root.join("agent");
        let cwd = root.join("project");
        let project_dir = cwd.join(".pi");
        let global = document(&agent_dir.join("settings.json"));
        let project = if trusted {
            document(&project_dir.join("settings.json"))
        } else {
            Map::new()
        };
        let base = |scope: &str| -> PathBuf {
            if scope == "project" {
                project_dir.clone()
            } else {
                agent_dir.clone()
            }
        };
        let packages: Vec<PackageInput> = package_inputs(
            project
                .get("packages")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice),
            global
                .get("packages")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice),
            base,
            |source, scope| match source {
                Source::Local { path } => local_path(path, &base(scope)),
                _ => PathBuf::from("/nonexistent-package"),
            },
        );
        let user = settings_lists(&global);
        let project_lists = settings_lists(&project);
        let input = ResolveInput {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            project_dir: project_dir.clone(),
            home: root.join("home"),
            project_trusted: trusted,
            packages,
            user: [&user[0][..], &user[1][..], &user[2][..], &user[3][..]],
            project: [
                &project_lists[0][..],
                &project_lists[1][..],
                &project_lists[2][..],
                &project_lists[3][..],
            ],
            builtins: &["mcp"],
        };
        let actual = as_json(&resolve(&input), &root);
        for kind in ResourceType::ALL {
            assert_eq!(
                actual[kind.key()],
                case[kind.key()],
                "{name}: {}",
                kind.key()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// pi's `readPiManifest` ignores a manifest list unless every entry is a
/// string.
#[test]
fn ignores_manifest_lists_with_non_strings() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("resolve-mixed-manifest");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(root.join("a.ts"), "").unwrap();
    std::fs::write(root.join("prompts/p.md"), "").unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"pi": {"extensions": ["./a.ts", 1], "prompts": ["./prompts/p.md"]}}"#,
    )
    .unwrap();
    let resources = package_resources(&root, None, true);
    assert!(enabled(&resources, ResourceType::Extensions).is_empty());
    assert_eq!(
        enabled(&resources, ResourceType::Prompts),
        [root.join("prompts/p.md")]
    );
}

/// The enabled resources of `kind`, as paths.
fn enabled(resolved: &ResolvedPaths, kind: ResourceType) -> Vec<PathBuf> {
    resolved
        .enabled(kind)
        .map(|info| PathBuf::from(&info.path))
        .collect()
}

fn write(root: &Path, files: &[(&str, &str)]) {
    let _ = std::fs::remove_dir_all(root);
    for (name, text) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

#[test]
fn package_resources_follow_manifests_conventional_dirs_and_filters() {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let root = tmp.join("package-manifest");
    write(
        &root,
        &[
            (
                "package.json",
                r#"{"pi": {"extensions": ["./src/*.ts", "!src/skip.ts", "packages/*/extensions"], "skills": ["./skills"]}}"#,
            ),
            ("src/a.ts", ""),
            ("src/skip.ts", ""),
            ("packages/x/extensions/tool/index.ts", ""),
            ("skills/x/SKILL.md", ""),
            ("prompts/p.md", ""),
        ],
    );
    let resources = package_resources(&root, None, false);
    // A glob that matches a folder takes the extensions in it.
    assert_eq!(
        enabled(&resources, ResourceType::Extensions),
        [
            root.join("src/a.ts"),
            root.join("packages/x/extensions/tool/index.ts")
        ]
    );
    assert_eq!(
        enabled(&resources, ResourceType::Skills),
        [root.join("skills/x/SKILL.md")]
    );
    // Only the types the manifest lists.
    assert!(enabled(&resources, ResourceType::Prompts).is_empty());

    let plain = tmp.join("package-conventional");
    write(
        &plain,
        &[
            ("extensions/a.ts", ""),
            ("extensions/b.js", ""),
            ("prompts/p.md", ""),
        ],
    );
    let resources = package_resources(&plain, None, false);
    assert_eq!(enabled(&resources, ResourceType::Extensions).len(), 2);
    assert_eq!(
        enabled(&resources, ResourceType::Prompts),
        [plain.join("prompts/p.md")]
    );
    let filtered = yapi_types::settings::FilteredPackage {
        source: String::new(),
        autoload: None,
        extensions: Some(vec!["extensions/a.ts".into()]),
        skills: None,
        prompts: Some(Vec::new()),
        themes: None,
    };
    let resources = package_resources(&plain, Some(&filtered), false);
    assert_eq!(
        enabled(&resources, ResourceType::Extensions),
        [plain.join("extensions/a.ts")]
    );
    assert!(enabled(&resources, ResourceType::Prompts).is_empty());

    // A folder with neither is one extension only as a local path.
    let bare = tmp.join("package-bare");
    write(&bare, &[("index.ts", "")]);
    assert_eq!(
        enabled(
            &package_resources(&bare, None, true),
            ResourceType::Extensions
        ),
        [bare.join("index.ts")]
    );
    assert!(
        enabled(
            &package_resources(&bare, None, false),
            ResourceType::Extensions
        )
        .is_empty()
    );
}
