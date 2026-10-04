//! Settings: global `settings.json` merged with the trusted project's.
//!
//! Port of `packages/coding-agent/src/core/settings-manager.ts` in pi `v1.0.0`.
//! Each scope is an order-preserving document; edits change it in place and write
//! it back the way pi does. Readers use the typed view of the merged document.

use std::path::{Path, PathBuf};

use ri_types::config::ConfigFile;
use ri_types::settings::Settings;
use serde_json::{Map, Value};

use crate::config::PROJECT_DIR;

/// Which file a setting lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `<agent>/settings.json`.
    Global,
    /// `<cwd>/.ri/settings.json`.
    Project,
}

/// Settings errors.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file is not valid settings JSON.
    #[error("Failed to parse {path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

/// Both scopes and their merged view.
#[derive(Clone, Debug)]
pub struct SettingsManager {
    global_path: PathBuf,
    project_path: PathBuf,
    global: Map<String, Value>,
    project: Map<String, Value>,
    project_trusted: bool,
    merged: Settings,
    /// Files that failed to load, by scope; they read as empty and are never
    /// written, so a broken file is not overwritten.
    broken: [bool; 2],
    errors: Vec<String>,
}

fn read_document(path: &Path) -> Result<Map<String, Value>, SettingsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(source) => {
            return Err(SettingsError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(SettingsError::Parse {
            path: path.to_path_buf(),
            message: "expected an object".into(),
        }),
        Err(err) => Err(SettingsError::Parse {
            path: path.to_path_buf(),
            message: err.to_string(),
        }),
    }
}

fn is_mergeable(value: &Value) -> bool {
    value.is_object()
}

/// pi's deep merge: objects merge recursively, anything else replaces.
fn deep_merge(base: &Map<String, Value>, overrides: &Map<String, Value>) -> Map<String, Value> {
    let mut result = base.clone();
    for (key, value) in overrides {
        let merged = match (base.get(key), value) {
            (Some(Value::Object(base)), Value::Object(over)) if is_mergeable(value) => {
                Value::Object(deep_merge(base, over))
            }
            _ => value.clone(),
        };
        result.insert(key.clone(), merged);
    }
    result
}

/// `defaultTools`: a list of plain names replaces the inherited one; a list of
/// only `+name` and `-name` entries is appended to it.
fn merge_default_tools(base: Option<&Value>, overrides: Option<&Value>) -> Option<Value> {
    let overrides = overrides?.as_array()?;
    let modifiers_only = overrides.iter().all(|entry| {
        entry
            .as_str()
            .is_some_and(|name| name.starts_with('+') || name.starts_with('-'))
    });
    if !modifiers_only {
        return Some(Value::Array(overrides.clone()));
    }
    let mut merged = base.and_then(Value::as_array).cloned().unwrap_or_default();
    merged.extend(overrides.iter().cloned());
    Some(Value::Array(merged))
}

impl SettingsManager {
    /// Loads both scopes; the project scope only when `project_trusted`.
    pub fn load(
        agent_dir: &Path,
        cwd: &Path,
        project_trusted: bool,
    ) -> Result<SettingsManager, SettingsError> {
        let global_path = agent_dir.join(ConfigFile::Settings.file_name());
        let project_path = cwd.join(PROJECT_DIR).join(ConfigFile::Settings.file_name());
        let mut errors = Vec::new();
        let mut broken = [false; 2];
        // pi reports a file that fails to load and carries on without it.
        let mut read = |path: &Path, scope: usize| match read_document(path) {
            Ok(document) => document,
            Err(error) => {
                let message = match error {
                    SettingsError::Parse { message, .. } => message,
                    SettingsError::Io { source, .. } => source.to_string(),
                };
                errors.push(format!(
                    "Invalid settings file {}: {message}",
                    path.display()
                ));
                broken[scope] = true;
                Map::new()
            }
        };
        let global = read(&global_path, 0);
        let project = if project_trusted {
            read(&project_path, 1)
        } else {
            Map::new()
        };
        let mut manager = SettingsManager {
            global_path,
            project_path,
            global,
            project,
            project_trusted,
            merged: Settings::default(),
            broken,
            errors,
        };
        manager.remerge();
        Ok(manager)
    }

    /// Settings with no files behind them.
    pub fn in_memory() -> SettingsManager {
        SettingsManager {
            global_path: PathBuf::new(),
            project_path: PathBuf::new(),
            global: Map::new(),
            project: Map::new(),
            project_trusted: false,
            merged: Settings::default(),
            broken: [false; 2],
            errors: Vec::new(),
        }
    }

    /// Why settings files failed to load, as pi's warnings word them.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    fn remerge(&mut self) {
        let mut merged = deep_merge(&self.global, &self.project);
        if let Some(tools) = merge_default_tools(
            self.global.get("defaultTools"),
            self.project.get("defaultTools"),
        ) {
            merged.insert("defaultTools".into(), tools);
        }
        // Unknown or invalid fields are ignored, as in pi.
        self.merged = serde_json::from_value(Value::Object(merged.clone())).unwrap_or_else(|_| {
            let mut lenient = Settings::default();
            for (key, value) in merged {
                let mut single = Map::new();
                single.insert(key, value);
                if let Ok(field) = serde_json::from_value::<Settings>(Value::Object(single)) {
                    lenient = merge_settings(lenient, field);
                }
            }
            lenient
        });
    }

    /// The merged settings.
    pub fn settings(&self) -> &Settings {
        &self.merged
    }

    /// pi's `getHttpIdleTimeoutMs`: `httpIdleTimeoutMs` in milliseconds, as a
    /// number, a numeric string or `"disabled"` (0); five minutes when unset
    /// or invalid.
    pub fn http_idle_timeout_ms(&self) -> u64 {
        let value = self
            .project
            .get("httpIdleTimeoutMs")
            .or_else(|| self.global.get("httpIdleTimeoutMs"));
        let number = match value {
            Some(Value::String(text)) if text.trim().eq_ignore_ascii_case("disabled") => Some(0.0),
            Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
            Some(Value::Number(number)) => number.as_f64(),
            _ => None,
        };
        number
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .map_or(300_000, |ms| ms.floor() as u64)
    }

    /// Whether the project scope is loaded.
    pub fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// The settings document of `scope` as written; empty for an untrusted
    /// project.
    pub fn document(&self, scope: Scope) -> &Map<String, Value> {
        match scope {
            Scope::Global => &self.global,
            Scope::Project => &self.project,
        }
    }

    /// Sets a top-level key in a scope and writes that file. `None` removes it.
    pub fn set(
        &mut self,
        scope: Scope,
        key: &str,
        value: Option<Value>,
    ) -> Result<(), SettingsError> {
        let (document, path, broken) = match scope {
            Scope::Global => (&mut self.global, &self.global_path, self.broken[0]),
            Scope::Project => (&mut self.project, &self.project_path, self.broken[1]),
        };
        match value {
            Some(value) => {
                document.insert(key.to_owned(), value);
            }
            None => {
                document.shift_remove(key);
            }
        }
        if !path.as_os_str().is_empty() && !broken {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|source| SettingsError::Io {
                    path: dir.to_path_buf(),
                    source,
                })?;
            }
            let text = ConfigFile::Settings
                .render(&Value::Object(document.clone()))
                .map_err(|err| SettingsError::Parse {
                    path: path.clone(),
                    message: err.to_string(),
                })?;
            std::fs::write(path, text).map_err(|source| SettingsError::Io {
                path: path.clone(),
                source,
            })?;
        }
        self.remerge();
        Ok(())
    }

    /// Sets `key` inside the object setting `field` of `scope`, keeping its
    /// other keys, and writes that scope's file.
    pub fn set_nested(
        &mut self,
        scope: Scope,
        field: &str,
        key: &str,
        value: Value,
    ) -> Result<(), SettingsError> {
        let document = match scope {
            Scope::Global => &self.global,
            Scope::Project => &self.project,
        };
        let mut object = match document.get(field) {
            Some(Value::Object(object)) => object.clone(),
            _ => Map::new(),
        };
        object.insert(key.to_owned(), value);
        self.set(scope, field, Some(Value::Object(object)))
    }
}

/// Fields of `over` that are set replace those of `base`.
fn merge_settings(base: Settings, over: Settings) -> Settings {
    let base = serde_json::to_value(base).unwrap_or(Value::Null);
    let over = serde_json::to_value(over).unwrap_or(Value::Null);
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            serde_json::from_value(Value::Object(deep_merge(&base, &over))).unwrap_or_default()
        }
        _ => Settings::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_like_pi() {
        let global = json!({"compaction": {"enabled": true, "reserveTokens": 100}, "defaultTools": ["read", "bash"], "theme": "dark"});
        let project =
            json!({"compaction": {"reserveTokens": 5}, "defaultTools": ["+grep", "-bash"]});
        let mut merged = deep_merge(global.as_object().unwrap(), project.as_object().unwrap());
        merged.insert(
            "defaultTools".into(),
            merge_default_tools(global.get("defaultTools"), project.get("defaultTools")).unwrap(),
        );
        assert_eq!(
            Value::Object(merged),
            json!({"compaction": {"enabled": true, "reserveTokens": 5}, "defaultTools": ["read", "bash", "+grep", "-bash"], "theme": "dark"})
        );
        assert_eq!(
            merge_default_tools(Some(&json!(["read"])), Some(&json!(["ls"]))),
            Some(json!(["ls"]))
        );
    }
}
