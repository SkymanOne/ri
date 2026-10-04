//! What an installed package provides: the resources its manifest lists, or
//! its conventional `extensions`, `skills`, `prompts` and `themes`
//! directories, narrowed by the settings entry's filters (pi's
//! `collectPackageResources`, as [`super::resolve`] ports it).

use std::path::{Path, PathBuf};

use ri_types::settings::FilteredPackage;

use super::resolve::{self, ResourceType};

/// A package's enabled resources.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageResources {
    /// Extension entry files.
    pub extensions: Vec<PathBuf>,
    /// Skill files.
    pub skills: Vec<PathBuf>,
    /// Prompt template files.
    pub prompts: Vec<PathBuf>,
    /// Theme files.
    pub themes: Vec<PathBuf>,
}

/// The enabled resources of the package installed at `root`, as its
/// settings `entry` filters them. `local` says whether its source is a local
/// path, the only kind pi takes as one extension when it is a folder with
/// neither a manifest nor resource folders.
pub fn package_resources(
    root: &Path,
    entry: Option<&FilteredPackage>,
    local: bool,
) -> PackageResources {
    let resolved = resolve::package_resources(root, entry, local);
    let enabled = |kind: ResourceType| {
        resolved
            .enabled(kind)
            .map(|info| PathBuf::from(&info.path))
            .collect()
    };
    PackageResources {
        extensions: enabled(ResourceType::Extensions),
        skills: enabled(ResourceType::Skills),
        prompts: enabled(ResourceType::Prompts),
        themes: enabled(ResourceType::Themes),
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
    fn manifests_conventional_dirs_and_filters() {
        let root = scratch("manifest");
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
            resources.extensions,
            [
                root.join("src/a.ts"),
                root.join("packages/x/extensions/tool/index.ts")
            ]
        );
        assert_eq!(resources.skills, [root.join("skills/x/SKILL.md")]);
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
        let resources = package_resources(&plain, None, false);
        assert_eq!(resources.extensions.len(), 2);
        assert_eq!(resources.prompts, [plain.join("prompts/p.md")]);
        let filtered = FilteredPackage {
            source: String::new(),
            autoload: None,
            extensions: Some(vec!["extensions/a.ts".into()]),
            skills: None,
            prompts: Some(Vec::new()),
            themes: None,
        };
        let resources = package_resources(&plain, Some(&filtered), false);
        assert_eq!(resources.extensions, [plain.join("extensions/a.ts")]);
        assert!(resources.prompts.is_empty());

        // A folder with neither is one extension only as a local path.
        let bare = scratch("bare");
        write(&bare, &[("index.ts", "")]);
        assert_eq!(
            package_resources(&bare, None, true).extensions,
            [bare.join("index.ts")]
        );
        assert!(package_resources(&bare, None, false).extensions.is_empty());
    }
}
