//! ri's resource resolver against `tests/fixtures/pi/resolve/cases.json`,
//! recorded from pi's `DefaultPackageManager.resolve` by
//! `tests/fixtures/pi/generator/resolve.mjs`.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};

use ri_core::packages::resolve::{
    PackageInput, ResolveInput, ResolvedPaths, ResourceType, package_inputs, resolve,
    settings_lists,
};
use ri_core::packages::source::{Source, local_path};
use serde_json::{Map, Value, json};

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
    let scratch = std::env::temp_dir().join(format!("ri-resolve-{}", std::process::id()));
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
