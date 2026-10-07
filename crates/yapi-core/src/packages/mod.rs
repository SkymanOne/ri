//! Packages: npm, git and local sources that bring extensions, skills,
//! prompt templates and themes. Port of pi's package manager
//! (`core/package-manager.ts` in pi `v1.0.0`), with a built-in npm client.
//!
//! Packages live under the scope's directory: `<agent>/npm` and
//! `<agent>/git/<host>/<path>` for the user, `<cwd>/.yapi/...` for a project.
//! Settings list them in `packages`.

pub mod npm;
pub mod resolve;
pub mod source;

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::Digest as _;
use yapi_types::settings::FilteredPackage;

use crate::config::PROJECT_DIR;
use crate::settings::{Scope, SettingsError, SettingsManager};
pub use source::{Source, parse};

/// Why a package operation failed; the message is pi's where pi has one.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// A failure with pi's message.
    #[error("{0}")]
    Message(String),
    /// Project settings cannot change while the project is untrusted.
    #[error("Project is not trusted. Use --approve to modify local package config.")]
    Untrusted,
    /// The npm client failed.
    #[error(transparent)]
    Npm(#[from] npm::NpmError),
    /// Settings could not be written.
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

/// A configured package, as `list` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configured {
    /// The source as written in settings.
    pub source: String,
    /// Whose settings list it.
    pub scope: Scope,
    /// Whether the entry filters the package's resources.
    pub filtered: bool,
    /// Where it is installed, when it is.
    pub installed_path: Option<PathBuf>,
    /// Its extension entry files after the entry's filters. Empty when it
    /// is not installed.
    pub extensions: Vec<PathBuf>,
}

/// Where `source`, written in settings whose directory is `base`, is or
/// would be installed.
pub fn install_location(source: &Source, base: &Path) -> PathBuf {
    match source {
        Source::Npm { name, .. } => base.join("npm").join("node_modules").join(name),
        Source::Git { host, path, .. } => base.join("git").join(host).join(path),
        Source::Local { path } => source::local_path(path, base),
    }
}

/// pi's `DefaultPackageManager.resolve` for the settings of `cwd` and
/// `agent_dir`: every resource of installed packages, settings entries and
/// the discovered directories, each enabled or not, with `builtins` as the
/// built-in extensions. Packages that are not installed are left out.
pub fn resolve_resources(
    cwd: &Path,
    agent_dir: &Path,
    settings: &SettingsManager,
    builtins: &[&str],
) -> resolve::ResolvedPaths {
    let project_dir = cwd.join(PROJECT_DIR);
    let base = |scope: &str| {
        if scope == "project" {
            project_dir.clone()
        } else {
            agent_dir.to_path_buf()
        }
    };
    let packages = resolve::package_inputs(
        &settings_packages(settings, Scope::Project),
        &settings_packages(settings, Scope::Global),
        base,
        |source, scope| install_location(source, &base(scope)),
    );
    let user = resolve::settings_lists(settings.document(Scope::Global));
    let project = resolve::settings_lists(settings.document(Scope::Project));
    resolve::resolve(&resolve::ResolveInput {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        project_dir: project_dir.clone(),
        home: crate::tools::path::home_dir(),
        project_trusted: settings.project_trusted(),
        packages,
        user: [&user[0], &user[1], &user[2], &user[3]],
        project: [&project[0], &project[1], &project[2], &project[3]],
        builtins,
    })
}

/// The `packages` entries of `scope`'s settings.
pub fn settings_packages(settings: &SettingsManager, scope: Scope) -> Vec<Value> {
    settings
        .document(scope)
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The directory a scope's packages and relative settings paths use: the
/// agent directory for the user, the project's config directory otherwise.
pub fn scope_dir(scope: Scope, cwd: &Path, agent_dir: &Path) -> PathBuf {
    match scope {
        Scope::Global => agent_dir.to_path_buf(),
        Scope::Project => cwd.join(PROJECT_DIR),
    }
}

/// A settings entry: a source string or a filtered package.
pub fn entry_source(entry: &Value) -> Option<&str> {
    entry.as_str().or_else(|| entry["source"].as_str())
}

/// Installs, removes and resolves packages for a working directory.
pub struct PackageManager {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: SettingsManager,
    npm: npm::Npm,
    progress: Box<dyn Fn(&str) + Send + Sync>,
}

impl std::fmt::Debug for PackageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageManager")
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

fn run_git(args: &[&str], dir: Option<&Path>) -> Result<(), PackageError> {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(crate::config::child_env());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command
        .output()
        .map_err(|err| PackageError::Message(format!("git {}: {err}", args[0])))?;
    if output.status.success() {
        return Ok(());
    }
    Err(PackageError::Message(format!(
        "git {} failed with code {}: {}",
        args[0],
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// pi's `resolveManagedPath`: `parts` resolved against `root`, which it
/// must not leave.
fn managed_path(root: &Path, parts: &Path) -> Result<PathBuf, PackageError> {
    let path = crate::tools::path::resolve_lexically(root, parts);
    if path.starts_with(root) {
        Ok(path)
    } else {
        Err(PackageError::Message(format!(
            "Refusing to use path outside package install root: {}",
            path.display()
        )))
    }
}

impl PackageManager {
    /// A manager for `cwd`, reading and writing `settings`, with packages from
    /// `registry`.
    pub fn new(
        cwd: PathBuf,
        agent_dir: PathBuf,
        settings: SettingsManager,
        registry: String,
    ) -> PackageManager {
        PackageManager {
            cwd,
            agent_dir,
            settings,
            npm: npm::Npm::new(registry),
            progress: Box::new(|_| {}),
        }
    }

    /// Reports the start of each install, removal and update.
    pub fn on_progress(&mut self, progress: impl Fn(&str) + Send + Sync + 'static) {
        self.progress = Box::new(progress);
    }

    /// The settings it reads and writes.
    pub fn settings(&self) -> &SettingsManager {
        &self.settings
    }

    fn base(&self, scope: Scope) -> PathBuf {
        scope_dir(scope, &self.cwd, &self.agent_dir)
    }

    fn npm_root(&self, scope: Scope) -> PathBuf {
        self.base(scope).join("npm")
    }

    /// Where `source`, as written in `scope`'s settings, is or would be
    /// installed.
    fn install_path(&self, source: &Source, scope: Scope) -> PathBuf {
        install_location(source, &self.base(scope))
    }

    /// Where `source`, as written in `scope`'s settings, is installed.
    pub fn installed_path(&self, source: &str, scope: Scope) -> Option<PathBuf> {
        let path = self.install_path(&parse(source), scope);
        path.exists().then_some(path)
    }

    fn assert_writable(&self, scope: Scope) -> Result<(), PackageError> {
        if scope == Scope::Project && !self.settings.project_trusted() {
            return Err(PackageError::Untrusted);
        }
        Ok(())
    }

    fn scope(local: bool) -> Scope {
        if local { Scope::Project } else { Scope::Global }
    }

    /// pi's `installAndPersist`: installs `source` into the user's (or with
    /// `local`, the project's) packages and lists it in their settings.
    pub async fn install(&mut self, source: &str, local: bool) -> Result<(), PackageError> {
        let scope = Self::scope(local);
        self.assert_writable(scope)?;
        (self.progress)(&format!("Installing {source}..."));
        let parsed = parse(source);
        match &parsed {
            Source::Npm { name, version, .. } => {
                self.install_npm(&self.npm_root(scope), name, version.as_deref())
                    .await?;
            }
            Source::Git {
                repo, reference, ..
            } => {
                let dir = self.install_path(&parsed, scope);
                self.install_git(repo, reference.as_deref(), &dir).await?;
            }
            Source::Local { path } => {
                let resolved = source::local_path(path, &self.cwd);
                if !resolved.exists() {
                    return Err(PackageError::Message(format!(
                        "Path does not exist: {}",
                        resolved.display()
                    )));
                }
            }
        }
        self.add_to_settings(source, scope)
    }

    /// Installs `name` (at `version`) into the npm root `root`.
    async fn install_npm(
        &mut self,
        root: &Path,
        name: &str,
        version: Option<&str>,
    ) -> Result<(), PackageError> {
        if let Some(command) = self.settings.settings().npm_command.clone() {
            // pi's `npmCommand` runs a real package manager, scripts and all.
            let (program, args) = command.split_first().ok_or_else(|| {
                PackageError::Message(
                    "Invalid npmCommand: first array entry must be a non-empty command".into(),
                )
            })?;
            let spec =
                version.map_or_else(|| name.to_owned(), |version| format!("{name}@{version}"));
            std::fs::create_dir_all(root).map_err(|err| PackageError::Message(err.to_string()))?;
            let status = std::process::Command::new(program)
                .envs(crate::config::child_env())
                .args(args)
                .args(["install", &spec, "--prefix"])
                .arg(root)
                .arg("--legacy-peer-deps")
                .status()
                .map_err(|err| PackageError::Message(format!("{program}: {err}")))?;
            if !status.success() {
                return Err(PackageError::Message(format!(
                    "{program} install {spec} failed with code {}",
                    status.code().unwrap_or(-1)
                )));
            }
            return Ok(());
        }
        npm::install(&mut self.npm, root, name, version).await?;
        Ok(())
    }

    async fn install_git(
        &mut self,
        repo: &str,
        reference: Option<&str>,
        dir: &Path,
    ) -> Result<(), PackageError> {
        if !dir.exists() {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| PackageError::Message(err.to_string()))?;
            }
            let target = dir.to_string_lossy().into_owned();
            if let Err(err) = run_git(&["clone", repo, &target], None) {
                self.remove_dir(dir);
                return Err(err);
            }
        }
        if let Some(reference) = reference
            && let Err(err) = run_git(&["checkout", reference], Some(dir))
        {
            self.remove_dir(dir);
            return Err(err);
        }
        npm::install_dependencies(&mut self.npm, dir).await?;
        Ok(())
    }

    /// Moves the checkout in `dir` to the latest commit of `reference`, else
    /// of its upstream branch, and installs its dependencies.
    async fn update_git(
        &mut self,
        dir: &Path,
        reference: Option<&str>,
    ) -> Result<(), PackageError> {
        match reference {
            Some(reference) => {
                run_git(&["fetch", "origin", reference], Some(dir))?;
                run_git(&["checkout", "FETCH_HEAD"], Some(dir))?;
            }
            None => {
                run_git(&["fetch", "--prune", "origin"], Some(dir))?;
                run_git(&["reset", "--hard", "@{upstream}"], Some(dir))?;
            }
        }
        npm::install_dependencies(&mut self.npm, dir).await?;
        Ok(())
    }

    /// Removes `dir` and the empty directories above it, up to the git root
    /// or the temporary packages folder.
    fn remove_dir(&self, dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        let temporary = self.agent_dir.join("tmp").join("extensions");
        let mut current = dir.parent();
        while let Some(parent) = current {
            if parent.file_name().is_some_and(|name| name == "git")
                || parent == temporary
                || std::fs::remove_dir(parent).is_err()
            {
                break;
            }
            current = parent.parent();
        }
    }

    /// pi's `removeAndPersist`: uninstalls `source` from the scope and drops
    /// its settings entries; `false` when settings did not list it.
    pub async fn remove(&mut self, source: &str, local: bool) -> Result<bool, PackageError> {
        let scope = Self::scope(local);
        self.assert_writable(scope)?;
        (self.progress)(&format!("Removing {source}..."));
        let parsed = parse(source);
        match &parsed {
            Source::Npm { name, .. } => npm::uninstall(&self.npm_root(scope), name)?,
            Source::Git { .. } => {
                let dir = self.install_path(&parsed, scope);
                if dir.exists() {
                    self.remove_dir(&dir);
                }
            }
            Source::Local { .. } => {}
        }
        self.remove_from_settings(source, scope)
    }

    fn packages(&self, scope: Scope) -> Vec<Value> {
        settings_packages(&self.settings, scope)
    }

    /// Whether settings entry `entry` of `scope` names the same package as
    /// command-line `source`.
    fn matches(&self, entry: &Value, source: &str, scope: Scope) -> bool {
        entry_source(entry).is_some_and(|existing| {
            parse(existing).identity(&self.base(scope)) == parse(source).identity(&self.cwd)
        })
    }

    /// The source as settings store it: a local path relative to the scope's
    /// directory.
    fn settings_source(&self, source: &str, scope: Scope) -> String {
        match parse(source) {
            Source::Local { path } => {
                let resolved = source::local_path(&path, &self.cwd);
                let relative = crate::tools::path::relative(&self.base(scope), &resolved);
                if relative.is_empty() {
                    ".".into()
                } else {
                    relative
                }
            }
            _ => source.to_owned(),
        }
    }

    fn add_to_settings(&mut self, source: &str, scope: Scope) -> Result<(), PackageError> {
        let normalized = self.settings_source(source, scope);
        let mut packages = self.packages(scope);
        match packages
            .iter()
            .position(|entry| self.matches(entry, source, scope))
        {
            Some(index) => {
                if entry_source(&packages[index]) == Some(normalized.as_str()) {
                    return Ok(());
                }
                if let Some(object) = packages[index].as_object_mut() {
                    object.insert("source".into(), Value::String(normalized));
                } else {
                    packages[index] = Value::String(normalized);
                }
            }
            None => packages.push(Value::String(normalized)),
        }
        self.settings
            .set(scope, "packages", Some(Value::Array(packages)))?;
        Ok(())
    }

    fn remove_from_settings(&mut self, source: &str, scope: Scope) -> Result<bool, PackageError> {
        let packages = self.packages(scope);
        let kept: Vec<Value> = packages
            .iter()
            .filter(|entry| !self.matches(entry, source, scope))
            .cloned()
            .collect();
        if kept.len() == packages.len() {
            return Ok(false);
        }
        self.settings
            .set(scope, "packages", Some(Value::Array(kept)))?;
        Ok(true)
    }

    /// The packages settings list: the user's, then the project's.
    pub fn list(&self) -> Vec<Configured> {
        [Scope::Global, Scope::Project]
            .into_iter()
            .flat_map(|scope| {
                self.packages(scope).into_iter().filter_map(move |entry| {
                    let source = entry_source(&entry)?.to_owned();
                    let installed_path = self.installed_path(&source, scope);
                    let filters: Option<FilteredPackage> = entry
                        .is_object()
                        .then(|| serde_json::from_value(entry.clone()).ok())
                        .flatten();
                    let extensions = installed_path
                        .as_deref()
                        .map(|root| {
                            resolve::package_resources(
                                root,
                                filters.as_ref(),
                                source::is_local(&source),
                            )
                            .enabled(resolve::ResourceType::Extensions)
                            .map(|info| PathBuf::from(&info.path))
                            .collect()
                        })
                        .unwrap_or_default();
                    Some(Configured {
                        installed_path,
                        filtered: entry.is_object(),
                        extensions,
                        source,
                        scope,
                    })
                })
            })
            .collect()
    }

    /// pi's `update`: reinstalls unpinned npm packages with newer matching
    /// versions and fast-forwards git packages, for every configured package
    /// or the one `source` names.
    pub async fn update(&mut self, source: Option<&str>) -> Result<(), PackageError> {
        let mut targets = Vec::new();
        for scope in [Scope::Global, Scope::Project] {
            for entry in self.packages(scope) {
                let Some(configured) = entry_source(&entry).map(str::to_owned) else {
                    continue;
                };
                if source.is_none_or(|source| self.matches(&entry, source, scope)) {
                    targets.push((configured, scope));
                }
            }
        }
        if let Some(source) = source
            && targets.is_empty()
        {
            return Err(PackageError::Message(format!(
                "No matching package found for {source}"
            )));
        }
        if crate::tools::external::offline() {
            return Ok(());
        }
        for (configured, scope) in targets {
            match parse(&configured) {
                Source::Npm {
                    name,
                    version,
                    pinned,
                } if !pinned => {
                    (self.progress)(&format!("Updating {configured}..."));
                    let range = version.as_deref().unwrap_or("latest");
                    let installed = self.install_path(&parse(&configured), scope);
                    let current = npm::installed_version(&installed);
                    let stale = match &current {
                        Some(current) => !npm::satisfies(current, range) || range == "latest",
                        None => true,
                    };
                    if stale {
                        self.install_npm(&self.npm_root(scope), &name, version.as_deref())
                            .await?;
                    }
                }
                Source::Git { reference, .. } => {
                    (self.progress)(&format!("Updating {configured}..."));
                    let dir = self.install_path(&parse(&configured), scope);
                    self.update_git(&dir, reference.as_deref()).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// pi's temporary package for `-e`: where `source` is for this run. npm
    /// and git sources live under `<agent>/tmp/extensions` and are not added
    /// to settings. A missing package is installed, an npm package whose
    /// installed version is outside the source's range is reinstalled, and an
    /// unpinned git checkout is refreshed, kept as it is when that fails.
    /// `offline` skips all three, and a missing package is then `None`.
    pub async fn install_temporary(
        &mut self,
        source: &str,
        offline: bool,
    ) -> Result<Option<PathBuf>, PackageError> {
        match parse(source) {
            Source::Npm { name, version, .. } => {
                let root = self.temporary_dir("npm", "", None)?;
                let dir = root.join("node_modules").join(&name);
                // A dist-tag such as `latest` is no range, so it never
                // reinstalls an installed package.
                let stale = npm::installed_version(&dir).is_none_or(|installed| {
                    version.as_deref().is_some_and(|range| {
                        npm::is_range(range) && !npm::satisfies(&installed, range)
                    })
                });
                if stale {
                    if offline {
                        return Ok(None);
                    }
                    self.install_npm(&root, &name, version.as_deref()).await?;
                }
                Ok(Some(dir))
            }
            Source::Git {
                repo,
                host,
                path,
                reference,
            } => {
                let dir =
                    self.temporary_dir(&format!("git-{host}"), &path, reference.as_deref())?;
                if !dir.exists() {
                    if offline {
                        return Ok(None);
                    }
                    self.install_git(&repo, reference.as_deref(), &dir).await?;
                } else if reference.is_none() && !offline {
                    let _ = self.update_git(&dir, None).await;
                }
                Ok(Some(dir))
            }
            Source::Local { path } => Ok(Some(source::local_path(&path, &self.cwd))),
        }
    }

    /// pi's `getTemporaryDir`: `<agent>/tmp/extensions/<prefix>/<hash>/<suffix>`,
    /// where `hash` starts the SHA-256 of `<prefix>-<suffix>[@<reference>]`.
    /// The `extensions` folder is readable by the user only.
    fn temporary_dir(
        &self,
        prefix: &str,
        suffix: &str,
        reference: Option<&str>,
    ) -> Result<PathBuf, PackageError> {
        let folder = self.agent_dir.join("tmp").join("extensions");
        let failed =
            |err: std::io::Error| PackageError::Message(format!("{}: {err}", folder.display()));
        std::fs::create_dir_all(&folder).map_err(failed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))
                .map_err(failed)?;
        }
        let key = match reference {
            Some(reference) => format!("{prefix}-{suffix}@{reference}"),
            None => format!("{prefix}-{suffix}"),
        };
        let hash = crate::time::hex(&sha2::Sha256::digest(key));
        let root = managed_path(&folder, Path::new(prefix))?;
        managed_path(&root, &Path::new(&hash[..8]).join(suffix))
    }

    /// pi's startup install: the configured npm and git packages that are
    /// missing, the project's first, each identity once. `on_error` receives
    /// the failures.
    pub async fn install_missing(&mut self, mut on_error: impl FnMut(String)) {
        let mut identities: Vec<String> = Vec::new();
        for scope in [Scope::Project, Scope::Global] {
            for entry in self.packages(scope) {
                let Some(configured) = entry_source(&entry).map(str::to_owned) else {
                    continue;
                };
                let parsed = parse(&configured);
                let identity = parsed.identity(&self.base(scope));
                if identities.contains(&identity) {
                    continue;
                }
                identities.push(identity);
                let root = self.install_path(&parsed, scope);
                let installed = match &parsed {
                    _ if root.exists() => Ok(()),
                    Source::Npm { name, version, .. } => {
                        self.install_npm(&self.npm_root(scope), name, version.as_deref())
                            .await
                    }
                    Source::Git {
                        repo, reference, ..
                    } => self.install_git(repo, reference.as_deref(), &root).await,
                    Source::Local { .. } => Ok(()),
                };
                if let Err(err) = installed {
                    on_error(format!("Failed to install {configured}: {err}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_dirs_are_pis() {
        let agent = std::env::temp_dir().join(format!("yapi-temporary-{}", std::process::id()));
        let manager = PackageManager::new(
            agent.clone(),
            agent.clone(),
            SettingsManager::in_memory(),
            String::new(),
        );
        let extensions = agent.join("tmp/extensions");
        let dir = |prefix: &str, suffix: &str, reference| {
            manager
                .temporary_dir(prefix, suffix, reference)
                .map_err(|err| err.to_string())
        };
        // Hashes from pi's `getTemporaryDir`.
        assert_eq!(
            dir("npm", "", None).unwrap(),
            extensions.join("npm/f35b2129")
        );
        assert_eq!(
            dir("git-github.com", "user/repo", None).unwrap(),
            extensions.join("git-github.com/338a1076/user/repo")
        );
        assert_eq!(
            dir("git-github.com", "user/repo", Some("v1")).unwrap(),
            extensions.join("git-github.com/18093e51/user/repo")
        );
        let escape = dir("git-github.com", "../../../outside", None).unwrap_err();
        assert!(escape.starts_with("Refusing to use path outside package install root: "));
        std::fs::remove_dir_all(&agent).unwrap();
    }
}
