//! What an installed package provides: the resources its manifest lists, or
//! its conventional `extensions`, `skills`, `prompts` and `themes`
//! directories, narrowed by the settings entry's filters (pi's
//! `collectPackageResources`).

use std::path::{Path, PathBuf};

use ri_types::settings::FilteredPackage;

use crate::extensions::discovery;
use crate::resources::Manifest;

/// A package's resources.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageResources {
    /// Extension entry files.
    pub extensions: Vec<PathBuf>,
    /// Skill files or directories.
    pub skills: Vec<PathBuf>,
    /// Prompt template files or directories.
    pub prompts: Vec<PathBuf>,
    /// Theme files or directories.
    pub themes: Vec<PathBuf>,
}

/// Whether `text` matches glob `pattern`: `*` within a path segment, `**`
/// across segments.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn segments(pattern: &[&str], text: &[&str]) -> bool {
        match (pattern.first(), text.first()) {
            (None, None) => true,
            (Some(&"**"), _) => {
                segments(&pattern[1..], text) || (!text.is_empty() && segments(pattern, &text[1..]))
            }
            (Some(part), Some(segment)) => {
                segment_match(part, segment) && segments(&pattern[1..], &text[1..])
            }
            _ => false,
        }
    }
    fn segment_match(pattern: &str, text: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == text,
            Some((prefix, rest)) => {
                text.starts_with(prefix)
                    && (0..=text.len() - prefix.len()).any(|skip| {
                        let tail = &text[prefix.len() + skip..];
                        text.is_char_boundary(prefix.len() + skip) && segment_match(rest, tail)
                    })
            }
        }
    }
    let pattern: Vec<&str> = pattern.trim_start_matches("./").split('/').collect();
    let text: Vec<&str> = text.split('/').collect();
    segments(&pattern, &text)
}

/// Files under `root` as paths relative to it, skipping dot entries and
/// `node_modules`.
fn files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if let Ok(relative) = path.strip_prefix(root) {
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// The paths manifest `entries` name: plain paths as they are, globs
/// expanded, and `!` patterns removing earlier matches.
fn manifest_paths(root: &Path, entries: &[String]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut all: Option<Vec<String>> = None;
    for entry in entries {
        if let Some(excluded) = entry.strip_prefix('!') {
            paths.retain(|path| {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                !glob_match(excluded, &relative)
            });
        } else if entry.contains('*') {
            let all = all.get_or_insert_with(|| files(root));
            paths.extend(
                all.iter()
                    .filter(|file| glob_match(entry, file))
                    .map(|file| root.join(file)),
            );
        } else {
            let path = crate::tools::path::resolve_lexically(root, Path::new(entry));
            if path.exists() {
                paths.push(path);
            }
        }
    }
    paths
}

/// Extension entry files a manifest path or directory stands for.
fn extension_entries(path: &Path) -> Vec<PathBuf> {
    if path.is_dir() {
        discovery::entries(path).unwrap_or_else(|| discovery::in_dir(path))
    } else {
        vec![path.to_path_buf()]
    }
}

/// Keeps the candidates `patterns` select: exact relative paths or globs,
/// with `!` exclusions; an empty list selects nothing.
fn filter(root: &Path, candidates: Vec<PathBuf>, patterns: &[String]) -> Vec<PathBuf> {
    let relative = |path: &Path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let includes: Vec<&str> = patterns
        .iter()
        .filter(|pattern| !pattern.starts_with('!'))
        .map(|pattern| pattern.trim_start_matches('+'))
        .collect();
    let excludes: Vec<&str> = patterns
        .iter()
        .filter_map(|pattern| pattern.strip_prefix('!'))
        .collect();
    candidates
        .into_iter()
        .filter(|path| {
            let text = relative(path);
            let included = includes.is_empty() && !excludes.is_empty()
                || includes.iter().any(|pattern| {
                    glob_match(pattern, &text)
                        || text.starts_with(&format!("{}/", pattern.trim_end_matches('/')))
                });
            included && !excludes.iter().any(|pattern| glob_match(pattern, &text))
        })
        .collect()
}

/// The resources of the package installed at `root`, as `entry` filters them.
pub fn package_resources(root: &Path, entry: Option<&FilteredPackage>) -> PackageResources {
    if root.is_file() {
        return PackageResources {
            extensions: vec![root.to_path_buf()],
            ..PackageResources::default()
        };
    }
    let manifest = Manifest::read(&root.join("package.json"));
    let conventional = |name: &str| {
        let dir = root.join(name);
        dir.is_dir().then_some(dir)
    };
    let mut resources = match &manifest {
        Some(manifest) => PackageResources {
            extensions: manifest_paths(root, &manifest.extensions)
                .iter()
                .flat_map(|path| extension_entries(path))
                .collect(),
            skills: manifest_paths(root, &manifest.skills),
            prompts: manifest_paths(root, &manifest.prompts),
            themes: manifest_paths(root, &manifest.themes),
        },
        None => PackageResources {
            extensions: conventional("extensions")
                .map(|dir| discovery::in_dir(&dir))
                .unwrap_or_default(),
            skills: conventional("skills").into_iter().collect(),
            prompts: conventional("prompts").into_iter().collect(),
            themes: conventional("themes").into_iter().collect(),
        },
    };
    let has_conventional = ["extensions", "skills", "prompts", "themes"]
        .iter()
        .any(|name| root.join(name).is_dir());
    if manifest.is_none() && !has_conventional {
        // A directory with neither is one extension.
        resources.extensions = extension_entries(root);
    }
    let Some(entry) = entry else {
        return resources;
    };
    let autoload = entry.autoload != Some(false);
    let apply = |candidates: Vec<PathBuf>, patterns: &Option<Vec<String>>| match patterns {
        Some(patterns) => filter(root, candidates, patterns),
        None if autoload => candidates,
        None => Vec::new(),
    };
    PackageResources {
        extensions: apply(resources.extensions, &entry.extensions),
        skills: apply(resources.skills, &entry.skills),
        prompts: apply(resources.prompts, &entry.prompts),
        themes: apply(resources.themes, &entry.themes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ri-package-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("src/*.ts", "src/a.ts"));
        assert!(!glob_match("src/*.ts", "src/x/a.ts"));
        assert!(glob_match("src/**/*.ts", "src/x/y/a.ts"));
        assert!(glob_match("**/*.md", "a.md"));
    }

    #[test]
    fn manifests_conventional_dirs_and_filters() {
        let root = scratch("manifest");
        write(
            &root,
            &[
                (
                    "package.json",
                    r#"{"pi": {"extensions": ["./src/*.ts", "!./src/skip.ts"], "skills": ["./skills"]}}"#,
                ),
                ("src/a.ts", ""),
                ("src/skip.ts", ""),
                ("skills/x/SKILL.md", ""),
                ("prompts/p.md", ""),
            ],
        );
        let resources = package_resources(&root, None);
        assert_eq!(resources.extensions, [root.join("src/a.ts")]);
        assert_eq!(resources.skills, [root.join("skills")]);
        // Only the types the manifest lists.
        assert!(resources.prompts.is_empty());

        let plain = scratch("conventional");
        write(
            &plain,
            &[
                ("extensions/a.ts", ""),
                ("extensions/b.js", ""),
                ("prompts/p.md", ""),
            ],
        );
        let resources = package_resources(&plain, None);
        assert_eq!(resources.extensions.len(), 2);
        assert_eq!(resources.prompts, [plain.join("prompts")]);
        let filtered = FilteredPackage {
            source: String::new(),
            autoload: None,
            extensions: Some(vec!["extensions/a.ts".into()]),
            skills: None,
            prompts: Some(Vec::new()),
            themes: None,
        };
        let resources = package_resources(&plain, Some(&filtered));
        assert_eq!(resources.extensions, [plain.join("extensions/a.ts")]);
        assert!(resources.prompts.is_empty());

        let bare = scratch("bare");
        write(&bare, &[("index.ts", "")]);
        assert_eq!(
            package_resources(&bare, None).extensions,
            [bare.join("index.ts")]
        );
    }
}
