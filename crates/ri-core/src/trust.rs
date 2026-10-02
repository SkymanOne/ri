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
        let data = self.read();
        canonical(cwd)
            .ancestors()
            .find_map(|dir| data.get(&*dir.to_string_lossy()).and_then(Value::as_bool))
    }

    /// Records a decision for `cwd`; `None` clears it. Keys are written sorted.
    pub fn set(&self, cwd: &Path, decision: Option<bool>) -> std::io::Result<()> {
        let mut data = self.read();
        let key = canonical(cwd).to_string_lossy().into_owned();
        match decision {
            Some(decision) => {
                data.insert(key, Value::Bool(decision));
            }
            None => {
                data.insert(key, Value::Null);
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
