//! Which files load as extensions; pi's `discoverAndLoadExtensions` path
//! rules (`core/extensions/loader.ts` in pi `v1.0.0`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::PROJECT_DIR;
use crate::resources::Manifest;
use crate::tools::path::resolve_to_cwd;

/// pi's extension files, and yapi's native extensions.
fn is_extension_file(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".js") || name.ends_with(".wasm")
}

/// Whether the extension entry `path` is a native extension, a WebAssembly
/// component, rather than a pi extension that runs in yapi-js.
pub fn is_native(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "wasm")
}

/// The entry points of extension directory `dir`: what its manifest declares,
/// else `index.ts` or `index.js`.
pub fn entries(dir: &Path) -> Option<Vec<PathBuf>> {
    if let Some(manifest) = Manifest::read(&dir.join("package.json")) {
        let declared: Vec<PathBuf> = manifest
            .extensions
            .iter()
            .map(|entry| crate::tools::path::resolve_lexically(dir, Path::new(entry)))
            .filter(|path| path.exists())
            .collect();
        if !declared.is_empty() {
            return Some(declared);
        }
    }
    ["index.ts", "index.js"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
        .map(|path| vec![path])
}

/// Extension files directly in `dir`, and the entry points of its
/// subdirectories. No deeper.
pub fn in_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    // Node's `readdirSync` lists entries sorted by name.
    let mut listed: Vec<_> = read.flatten().collect();
    listed.sort_by_key(std::fs::DirEntry::file_name);
    let mut found = Vec::new();
    for entry in listed {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if (kind.is_file() || kind.is_symlink()) && is_extension_file(&name) {
            found.push(path);
            continue;
        }
        if (kind.is_dir() || kind.is_symlink())
            && let Some(entries) = entries(&path)
        {
            found.extend(entries);
        }
    }
    found
}

/// The extension files `paths` name: a directory loads its entry points, or
/// failing those, the files in it.
pub fn configured(paths: &[String], cwd: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for path in paths {
        let resolved = resolve_to_cwd(path, cwd);
        if resolved.is_dir() {
            found.extend(entries(&resolved).unwrap_or_else(|| in_dir(&resolved)));
        } else {
            found.push(resolved);
        }
    }
    found
}

/// The extensions installed in the project's `.yapi/extensions`, when the
/// project is trusted, then in the agent directory's `extensions`.
pub fn installed(cwd: &Path, agent_dir: &Path, project_trusted: bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if project_trusted {
        found.extend(in_dir(&cwd.join(PROJECT_DIR).join("extensions")));
    }
    found.extend(in_dir(&agent_dir.join("extensions")));
    found
}

/// `paths` without repeats, in order.
pub fn unique(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// pi's `discoverAndLoadExtensions` order: the installed extensions of a
/// trusted project and the agent directory, then `configured` paths.
pub fn discover(configured_paths: &[String], cwd: &Path, agent_dir: &Path) -> Vec<PathBuf> {
    unique(
        installed(cwd, agent_dir, true)
            .into_iter()
            .chain(configured(configured_paths, cwd)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_pi_discovery_rules() {
        let root = std::env::temp_dir().join(format!("yapi-discovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |name: &str, text: &str| {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("cwd/.yapi/extensions/a.ts", "");
        write("cwd/.yapi/extensions/notes.md", "");
        write("cwd/.yapi/extensions/b/index.ts", "");
        write("cwd/.yapi/extensions/b/deep/c.ts", "");
        write(
            "agent/extensions/pkg/package.json",
            r#"{"pi":{"extensions":["./src/main.ts","./missing.ts"]}}"#,
        );
        write("agent/extensions/pkg/src/main.ts", "");
        write("agent/extensions/pkg/index.ts", "");
        write("cwd/loose/x.js", "");
        write("cwd/loose/y.ts", "");
        let cwd = root.join("cwd");
        let mut found = discover(
            &["loose".into(), "./.yapi/extensions/a.ts".into()],
            &cwd,
            &root.join("agent"),
        );
        let relative: Vec<String> = found
            .drain(..)
            .map(|path| {
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let mut local: Vec<&String> = relative.iter().take(2).collect();
        local.sort();
        assert_eq!(
            local,
            [
                "cwd/.yapi/extensions/a.ts",
                "cwd/.yapi/extensions/b/index.ts"
            ]
        );
        assert_eq!(relative[2], "agent/extensions/pkg/src/main.ts");
        let mut loose: Vec<&String> = relative[3..].iter().collect();
        loose.sort();
        assert_eq!(loose, ["cwd/loose/x.js", "cwd/loose/y.ts"]);
    }
}
