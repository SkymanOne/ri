//! Which files load as extensions; pi's `discoverAndLoadExtensions` path
//! rules (`core/extensions/loader.ts` in pi `v1.0.0`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::PROJECT_DIR;
use crate::resources::Manifest;
use crate::tools::path::resolve_to_cwd;

fn is_extension_file(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".js")
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
    let mut found = Vec::new();
    for entry in read.flatten() {
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

/// The extension files to load, in order: the project's `.ri/extensions`, the
/// agent directory's `extensions`, then `configured` paths. A configured
/// directory loads its entry points, or failing those, the files in it.
pub fn discover(configured: &[String], cwd: &Path, agent_dir: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    let mut add = |found: Vec<PathBuf>| {
        for path in found {
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
    };
    add(in_dir(&cwd.join(PROJECT_DIR).join("extensions")));
    add(in_dir(&agent_dir.join("extensions")));
    for path in configured {
        let resolved = resolve_to_cwd(path, cwd);
        if resolved.is_dir() {
            match entries(&resolved) {
                Some(found) => add(found),
                None => add(in_dir(&resolved)),
            }
        } else {
            add(vec![resolved]);
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_pi_discovery_rules() {
        let root = std::env::temp_dir().join(format!("ri-discovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |name: &str, text: &str| {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("cwd/.ri/extensions/a.ts", "");
        write("cwd/.ri/extensions/notes.md", "");
        write("cwd/.ri/extensions/b/index.ts", "");
        write("cwd/.ri/extensions/b/deep/c.ts", "");
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
            &["loose".into(), "./.ri/extensions/a.ts".into()],
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
            ["cwd/.ri/extensions/a.ts", "cwd/.ri/extensions/b/index.ts"]
        );
        assert_eq!(relative[2], "agent/extensions/pkg/src/main.ts");
        let mut loose: Vec<&String> = relative[3..].iter().collect();
        loose.sort();
        assert_eq!(loose, ["cwd/loose/x.js", "cwd/loose/y.ts"]);
    }
}
