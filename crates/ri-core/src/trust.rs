//! Project trust: whether ri may load a project's `.ri` settings and resources.
//!
//! Port of `trust-manager.ts` and `project-trust.ts` in pi `v1.0.0`. Decisions live
//! in `<agent>/trust.json`, keyed by directory; the nearest decided ancestor wins.

use std::path::{Path, PathBuf};

use ri_types::settings::DefaultProjectTrust;
use serde_json::{Map, Value};

use crate::config::PROJECT_DIR;
use crate::tools::path::home_dir;

const TRUST_REQUIRING: &[&str] = &[
    "settings.json",
    "mcp.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `cwd` has project resources that need trust: anything in `.ri/` that
/// changes behavior, or an `.agents/skills` directory in it or an ancestor (other
/// than the user's own).
pub fn requires_trust(cwd: &Path) -> bool {
    let cwd = canonical(cwd);
    let config = cwd.join(PROJECT_DIR);
    if TRUST_REQUIRING
        .iter()
        .any(|entry| config.join(entry).exists())
    {
        return true;
    }
    let user_skills = canonical(&home_dir()).join(".agents").join("skills");
    cwd.ancestors().any(|dir| {
        let skills = dir.join(".agents").join("skills");
        skills != user_skills && skills.exists()
    })
}

/// Stored trust decisions.
#[derive(Clone, Debug)]
pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    /// The store in `agent_dir`.
    pub fn new(agent_dir: &Path) -> TrustStore {
        TrustStore {
            path: agent_dir.join("trust.json"),
        }
    }

    fn read(&self) -> Map<String, Value> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| {
                serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok()
            })
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }

    /// The decision for `cwd` from it or its nearest decided ancestor.
    pub fn get(&self, cwd: &Path) -> Option<bool> {
        self.entry(cwd).map(|(_, decision)| decision)
    }

    /// The nearest decided directory at or above `cwd`, and its decision.
    pub fn entry(&self, cwd: &Path) -> Option<(PathBuf, bool)> {
        let data = self.read();
        canonical(cwd).ancestors().find_map(|dir| {
            data.get(&*dir.to_string_lossy())
                .and_then(Value::as_bool)
                .map(|decision| (dir.to_path_buf(), decision))
        })
    }

    /// Records a decision for `cwd`; `None` clears it. Keys are written sorted.
    pub fn set(&self, cwd: &Path, decision: Option<bool>) -> std::io::Result<()> {
        self.set_many(&[(cwd.to_path_buf(), decision)])
    }

    /// Records several decisions at once, as pi's `setMany`.
    pub fn set_many(&self, updates: &[(PathBuf, Option<bool>)]) -> std::io::Result<()> {
        let mut data = self.read();
        for (path, decision) in updates {
            let key = canonical(path).to_string_lossy().into_owned();
            match decision {
                Some(decision) => {
                    data.insert(key, Value::Bool(*decision));
                }
                None => {
                    data.remove(&key);
                }
            }
        }
        data.sort_keys();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = ri_types::json::to_string_pretty(&Value::Object(data), "  ")
            .map_err(std::io::Error::other)?;
        std::fs::write(&self.path, text + "\n")
    }
}

/// A choice of the trust prompt and `/trust`: pi's `ProjectTrustOption`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustOption {
    /// What the option says.
    pub label: String,
    /// Whether choosing it trusts the project.
    pub trusted: bool,
    /// The decisions it stores; `None` clears one.
    pub updates: Vec<(PathBuf, Option<bool>)>,
    /// The directory whose stored decision it stands for.
    pub saved_path: Option<PathBuf>,
}

/// pi's `getProjectTrustOptions`: trust the project or its parent, or not;
/// with `session_only`, also trust or distrust for this session only.
pub fn trust_options(cwd: &Path, session_only: bool) -> Vec<TrustOption> {
    let path = canonical(cwd);
    let option = |label: String, trusted: bool, updates: Vec<(PathBuf, Option<bool>)>| {
        let saved_path = updates.first().map(|(path, _)| path.clone());
        TrustOption {
            label,
            trusted,
            updates,
            saved_path,
        }
    };
    let mut options = vec![option(
        "Trust".into(),
        true,
        vec![(path.clone(), Some(true))],
    )];
    if let Some(parent) = path.parent() {
        options.push(option(
            format!("Trust parent folder ({})", parent.display()),
            true,
            vec![(parent.to_path_buf(), Some(true)), (path.clone(), None)],
        ));
    }
    if session_only {
        options.push(option("Trust (this session only)".into(), true, Vec::new()));
    }
    options.push(option(
        "Do not trust".into(),
        false,
        vec![(path.clone(), Some(false))],
    ));
    if session_only {
        options.push(option(
            "Do not trust (this session only)".into(),
            false,
            Vec::new(),
        ));
    }
    options
}

/// The title of the startup trust prompt for `cwd`.
pub fn prompt_title(cwd: &Path) -> String {
    format!(
        "Trust project folder?\n{}\n\nThis allows ri to load {PROJECT_DIR} settings and resources, install missing project packages, and execute project extensions.",
        canonical(cwd).display()
    )
}

/// Whether startup asks whether to trust `cwd`: it has resources needing
/// trust and neither an override, a stored decision nor the
/// `defaultProjectTrust` setting decides.
pub fn needs_prompt(
    cwd: &Path,
    store: &TrustStore,
    override_: Option<bool>,
    default: Option<DefaultProjectTrust>,
) -> bool {
    override_.is_none()
        && requires_trust(cwd)
        && store.get(cwd).is_none()
        && !matches!(
            default,
            Some(DefaultProjectTrust::Always | DefaultProjectTrust::Never)
        )
}

/// Resolves trust without a UI: an explicit override, no resources needing trust,
/// a stored decision, then the `defaultProjectTrust` setting; `ask` without a UI
/// means untrusted.
pub fn resolve_trusted(
    cwd: &Path,
    store: &TrustStore,
    override_: Option<bool>,
    default: Option<DefaultProjectTrust>,
) -> bool {
    if let Some(trusted) = override_ {
        return trusted;
    }
    if !requires_trust(cwd) {
        return true;
    }
    if let Some(decision) = store.get(cwd) {
        return decision;
    }
    matches!(default, Some(DefaultProjectTrust::Always))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_are_stored_inherited_and_cleared() {
        let root = std::env::temp_dir().join(format!("ri-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("work").join("app");
        std::fs::create_dir_all(&project).unwrap();
        let store = TrustStore::new(&root.join("agent"));
        assert_eq!(store.entry(&project), None);

        let parent = trust_options(&project, false)
            .into_iter()
            .find(|option| option.label.starts_with("Trust parent folder"))
            .unwrap();
        store.set(&project, Some(false)).unwrap();
        store.set_many(&parent.updates).unwrap();
        let parent_dir = canonical(&root.join("work"));
        assert_eq!(store.entry(&project), Some((parent_dir.clone(), true)));
        let text = std::fs::read_to_string(root.join("agent").join("trust.json")).unwrap();
        assert_eq!(
            text,
            format!("{{\n  \"{}\": true\n}}\n", parent_dir.display())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn options_follow_pi() {
        let labels: Vec<String> = trust_options(Path::new("/tmp"), true)
            .into_iter()
            .map(|option| option.label)
            .collect();
        let root = canonical(Path::new("/tmp"));
        let parent = root.parent().map(|parent| parent.display().to_string());
        let mut expected = vec!["Trust".to_owned()];
        expected.extend(parent.map(|parent| format!("Trust parent folder ({parent})")));
        expected.extend(
            [
                "Trust (this session only)",
                "Do not trust",
                "Do not trust (this session only)",
            ]
            .map(str::to_owned),
        );
        assert_eq!(labels, expected);
        assert!(!needs_prompt(
            Path::new("/"),
            &TrustStore::new(Path::new("/nonexistent")),
            Some(true),
            None
        ));
    }
}
