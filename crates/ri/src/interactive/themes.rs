//! Theme files registered by the name they declare, as pi's resource loader
//! and theme registry do: the first file to declare a name wins.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ri_tui::color::ColorMode;
use ri_tui::theme::Theme;
use ri_types::rpc::SourceInfo;

/// A problem with a theme path, shown under `[Theme conflicts]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    /// A path that cannot be read, or a file that is not a valid theme.
    Warning {
        /// What went wrong.
        message: String,
        /// The path.
        path: PathBuf,
    },
    /// A file declaring a name an earlier file declared; it is skipped.
    Collision {
        /// The declared name.
        name: String,
        /// The file that keeps the name, with its source.
        winner: SourceInfo,
        /// The skipped file.
        loser: PathBuf,
    },
}

/// The registered theme files of a session.
#[derive(Clone, Debug, Default)]
pub struct ThemeFiles {
    named: Vec<(String, SourceInfo)>,
    /// Problems found while loading, in order.
    pub diagnostics: Vec<Diagnostic>,
}

impl ThemeFiles {
    /// Registers the JSON files at `paths`, and those directly inside
    /// directories among them, in order. A file in a directory has the
    /// directory's source.
    pub fn load(paths: &[SourceInfo]) -> ThemeFiles {
        let mut files = ThemeFiles::default();
        let mut seen = HashSet::new();
        for source in paths {
            let path = &PathBuf::from(&source.path);
            if !seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())) {
                continue;
            }
            if !path.exists() {
                files.warn("theme path does not exist", path);
            } else if path.is_dir() {
                let Ok(entries) = std::fs::read_dir(path) else {
                    files.warn("failed to read theme directory", path);
                    continue;
                };
                let mut found: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|file| {
                        file.is_file() && file.extension().is_some_and(|ext| ext == "json")
                    })
                    .collect();
                found.sort();
                for file in found {
                    files.register(file, source);
                }
            } else if path.extension().is_some_and(|ext| ext == "json") {
                files.register(path.clone(), source);
            } else {
                files.warn("theme path is not a json file", path);
            }
        }
        files
    }

    /// The file that declares `name`.
    pub fn path(&self, name: &str) -> Option<&Path> {
        self.named
            .iter()
            .find(|(declared, _)| declared == name)
            .map(|(_, source)| Path::new(&source.path))
    }

    fn warn(&mut self, message: &str, path: &Path) {
        self.diagnostics.push(Diagnostic::Warning {
            message: message.to_owned(),
            path: path.to_path_buf(),
        });
    }

    fn register(&mut self, path: PathBuf, source: &SourceInfo) {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                Theme::from_json_lenient(&path.display().to_string(), &text, ColorMode::TrueColor)
                    .map_err(|error| error.to_string())
            });
        let theme = match parsed {
            Ok(theme) => theme,
            Err(message) => {
                self.diagnostics.push(Diagnostic::Warning { message, path });
                return;
            }
        };
        let name = theme.name.unwrap_or_else(|| "unnamed".to_owned());
        if name.contains('/') {
            self.diagnostics.push(Diagnostic::Warning {
                message: format!(
                    "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
                ),
                path,
            });
            return;
        }
        match self.named.iter().find(|(declared, _)| *declared == name) {
            Some((_, winner)) => {
                let winner = winner.clone();
                self.diagnostics.push(Diagnostic::Collision {
                    name,
                    winner,
                    loser: path,
                });
            }
            None => {
                let source = SourceInfo {
                    path: path.to_string_lossy().into_owned(),
                    ..source.clone()
                };
                self.named.push((name, source));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(name: &str) -> String {
        let mut json: serde_json::Value = serde_json::from_str(ri_tui::theme::DARK_THEME).unwrap();
        json["name"] = serde_json::Value::String(name.to_owned());
        json.to_string()
    }

    #[test]
    fn first_declaration_wins_and_problems_are_reported() {
        let root = std::env::temp_dir().join(format!("ri-themes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = root.join("a");
        let second = root.join("b");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("ocean.json"), theme("ocean")).unwrap();
        std::fs::write(second.join("other.json"), theme("ocean")).unwrap();
        std::fs::write(second.join("broken.json"), "{").unwrap();
        std::fs::write(second.join("notes.txt"), "").unwrap();
        let single = root.join("forest.json");
        std::fs::write(&single, theme("forest")).unwrap();
        let missing = root.join("missing");

        let source = |path: &Path| SourceInfo {
            path: path.to_string_lossy().into_owned(),
            source: "local".into(),
            scope: "temporary".into(),
            origin: "top-level".into(),
            base_dir: None,
        };
        let files = ThemeFiles::load(&[
            source(&first),
            source(&second),
            source(&single),
            source(&missing),
            source(&first),
            source(&second.join("notes.txt")),
        ]);

        assert_eq!(
            files.path("ocean"),
            Some(first.join("ocean.json").as_path())
        );
        assert_eq!(files.path("forest"), Some(single.as_path()));
        assert_eq!(files.path("dark"), None);
        let kinds: Vec<String> = files
            .diagnostics
            .iter()
            .map(|diagnostic| match diagnostic {
                Diagnostic::Warning { message, path } => format!(
                    "warning {} {}",
                    path.file_name().unwrap().to_string_lossy(),
                    message.lines().next().unwrap_or_default().contains("theme")
                ),
                Diagnostic::Collision {
                    name,
                    winner,
                    loser,
                } => format!(
                    "collision {name} {} {}",
                    Path::new(&winner.path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy(),
                    loser.file_name().unwrap().to_string_lossy()
                ),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "warning broken.json true",
                "collision ocean ocean.json other.json",
                "warning missing true",
                "warning notes.txt true",
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
