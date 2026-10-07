//! A built-in npm client, so packages install without Node: it resolves
//! versions from the registry, verifies and unpacks tarballs into
//! `node_modules` and installs dependencies, hoisting them where Node's
//! resolution allows. It runs no lifecycle scripts, so native addons stay
//! unbuilt; an extension that loads one fails then.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sha2::Digest as _;

/// Packages yapi provides to extensions itself; installing them is wasted work.
const HOST_PACKAGES: [&str; 10] = [
    "@earendil-works/pi-coding-agent",
    "@earendil-works/pi-ai",
    "@earendil-works/pi-agent-core",
    "@earendil-works/pi-tui",
    "@mariozechner/pi-coding-agent",
    "@mariozechner/pi-ai",
    "@mariozechner/pi-agent-core",
    "@mariozechner/pi-tui",
    "typebox",
    "@sinclair/typebox",
];

/// Why an npm operation failed.
#[derive(Debug, thiserror::Error)]
pub enum NpmError {
    /// The registry could not be reached or answered with an error.
    #[error("{0}")]
    Registry(String),
    /// No published version matches.
    #[error("No version of {name} matches {range}")]
    NoMatch {
        /// The package.
        name: String,
        /// The requested range.
        range: String,
    },
    /// The tarball's digest differs from the registry's.
    #[error("Integrity check failed for {0}")]
    Integrity(String),
    /// A file operation failed.
    #[error("{path}: {source}")]
    Io {
        /// Where.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> NpmError + '_ {
    move |source| NpmError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// npm's configuration as the `npm install` pi runs reads it: the user config
/// file (`npm_config_userconfig`, else `~/.npmrc`) under `npm_config_*`
/// environment variables, with `${VAR}` references expanded.
pub fn config() -> HashMap<String, String> {
    let env = |name: &str| std::env::var(name).ok();
    let vars: HashMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| {
            let key = env_key(name.to_str()?)?;
            let value = expand(value.to_str()?, env);
            (!value.is_empty()).then_some((key, value))
        })
        .collect();
    let file = vars.get("userconfig").map_or_else(
        || crate::tools::path::home_dir().join(".npmrc"),
        |path| PathBuf::from(crate::tools::path::expand_home(path)),
    );
    let mut config = parse_config(&std::fs::read_to_string(file).unwrap_or_default(), env);
    config.extend(vars);
    config
}

/// The setting environment variable `name` holds, when it is an
/// `npm_config_*` variable, named as npm names it.
fn env_key(name: &str) -> Option<String> {
    let key = name
        .get(..11)
        .filter(|prefix| prefix.eq_ignore_ascii_case("npm_config_"))
        .and(name.get(11..))?;
    // npm keeps `//host/:_authToken` keys as written.
    if key.starts_with("//") {
        return Some(key.to_owned());
    }
    let rest = key.get(1..)?.replace('_', "-");
    Some(format!("{}{rest}", &key[..1]).to_lowercase())
}

/// The top-level settings of an npmrc file, read as npm's `ini` parser reads
/// them, with `${VAR}` references in keys and values expanded from `env`.
fn parse_config(text: &str, env: impl Fn(&str) -> Option<String>) -> HashMap<String, String> {
    let mut config = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            // What follows belongs to a section, which npm ignores.
            break;
        }
        if line.starts_with([';', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let quoted = value.len() >= 2
            && (value.starts_with('"') && value.ends_with('"')
                || value.starts_with('\'') && value.ends_with('\''));
        let value = if quoted {
            &value[1..value.len() - 1]
        } else {
            value.split([';', '#']).next().unwrap_or_default().trim()
        };
        let value = expand(value, &env);
        if !value.is_empty() {
            config.insert(expand(key.trim(), &env), value);
        }
    }
    config
}

/// `text` with npm's `${VAR}` references replaced by `env`'s values. An
/// undefined `${VAR}` stays as written and an undefined `${VAR?}` is empty.
/// npm's backslash escapes of references are not supported.
fn expand(text: &str, env: impl Fn(&str) -> Option<String>) -> String {
    let mut expanded = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        rest = &rest[start..];
        let reference = rest[2..].find('}').map(|end| &rest[2..end + 2]);
        let Some(reference) = reference.filter(|name| {
            let name = name.strip_suffix('?').unwrap_or(name);
            !name.is_empty() && !name.contains(['$', '{', '?'])
        }) else {
            expanded.push_str("${");
            rest = &rest[2..];
            continue;
        };
        let (name, optional) = match reference.strip_suffix('?') {
            Some(name) => (name, true),
            None => (reference, false),
        };
        match env(name) {
            Some(value) => expanded.push_str(&value),
            None if optional => {}
            None => expanded.push_str(&rest[..reference.len() + 3]),
        }
        rest = &rest[reference.len() + 3..];
    }
    expanded.push_str(rest);
    expanded
}

/// `token` without the `v` node-semver allows before a version, as in
/// `v1.2.3` or `^v1.2`.
fn without_v(token: &str) -> String {
    let version = token.trim_start_matches(['<', '>', '=', '~', '^']);
    let operator = &token[..token.len() - version.len()];
    match version.strip_prefix('v') {
        Some(version) => format!("{operator}{version}"),
        None => token.to_owned(),
    }
}

/// npm range syntax as `semver` requirements: any alternative may match.
fn requirements(range: &str) -> Option<Vec<semver::VersionReq>> {
    range
        .split("||")
        .map(|alternative| {
            let alternative = alternative.trim();
            let text = if let Some((low, high)) = alternative.split_once(" - ") {
                format!(">={}, <={}", without_v(low.trim()), without_v(high.trim()))
            } else {
                // npm separates comparators with spaces; `semver` with commas.
                let mut parts: Vec<String> = Vec::new();
                for token in alternative.split_whitespace() {
                    let token = without_v(token);
                    match parts.last_mut() {
                        Some(last)
                            if matches!(
                                last.as_str(),
                                ">" | ">=" | "<" | "<=" | "=" | "~" | "^"
                            ) =>
                        {
                            last.push_str(&token);
                        }
                        // A bare version is exact in npm and a caret range in
                        // `semver`. Wildcards such as `1.x` mean the same in both.
                        _ if token.starts_with(|c: char| c.is_ascii_digit())
                            && !token.contains(['x', 'X', '*']) =>
                        {
                            parts.push(format!("={token}"));
                        }
                        _ => parts.push(token),
                    }
                }
                if parts.is_empty() {
                    "*".into()
                } else {
                    parts.join(", ")
                }
            };
            semver::VersionReq::parse(&text).ok()
        })
        .collect()
}

/// Whether installed `version` satisfies `range`.
pub fn satisfies(version: &str, range: &str) -> bool {
    let Ok(version) = semver::Version::parse(version) else {
        return false;
    };
    requirements(range)
        .is_some_and(|alternatives| alternatives.iter().any(|req| req.matches(&version)))
}

/// What npm asks a registry for: the abbreviated packument, which holds only
/// what installs need. Full packuments of long-lived packages run to tens of
/// megabytes.
const PACKUMENT_ACCEPT: &str =
    "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";

/// A registry client with a per-run packument cache.
pub struct Npm {
    config: HashMap<String, String>,
    packuments: HashMap<String, Value>,
}

impl Npm {
    /// A client with npm's configuration `config`, as [`config`] reads it.
    pub fn new(config: HashMap<String, String>) -> Npm {
        Npm {
            config,
            packuments: HashMap::new(),
        }
    }

    /// The registry of package `name`, ending in `/`: its scope's
    /// `@scope:registry`, else `registry`, else npm's default.
    fn registry(&self, name: &str) -> String {
        let scope = name.split_once('/').map(|(scope, _)| scope);
        let registry = scope
            .filter(|scope| scope.starts_with('@'))
            .and_then(|scope| self.config.get(&format!("{scope}:registry")))
            .or_else(|| self.config.get("registry"))
            .map_or("https://registry.npmjs.org/", String::as_str);
        format!("{}/", registry.strip_suffix('/').unwrap_or(registry))
    }

    /// The `Authorization` npm sends with a request for `url` from
    /// `registry`: the credentials of the longest `//host/path` key that
    /// covers `url`, else those of `registry` when it is on `url`'s host.
    fn authorization(&self, url: &str, registry: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        let host =
            |url: &url::Url| url[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
        let mut key = format!("//{}{}", host(&parsed), parsed.path());
        while key.len() > 2 {
            let field = |name: &str| self.config.get(&format!("{key}:{name}"));
            if let Some(token) = field("_authToken") {
                return Some(format!("Bearer {token}"));
            }
            if let Some(auth) = field("_auth") {
                return Some(format!("Basic {auth}"));
            }
            if let (Some(username), Some(password)) = (field("username"), field("_password")) {
                let mut credentials = format!("{username}:").into_bytes();
                credentials.extend(STANDARD.decode(password).ok()?);
                return Some(format!("Basic {}", STANDARD.encode(credentials)));
            }
            // Up one level: `//host/a/b/` to `//host/a/b`, then `//host/a/`.
            let parent = match key.strip_suffix('/') {
                Some(parent) => parent.len(),
                None => key.trim_end_matches(|c| c != '/').len(),
            };
            key.truncate(parent);
        }
        let same_host =
            url::Url::parse(registry).is_ok_and(|registry| host(&registry) == host(&parsed));
        if url != registry && same_host {
            self.authorization(registry, registry)
        } else {
            None
        }
    }

    /// GETs `url` of a package from `registry`, asking for the `accept`
    /// media types.
    async fn get(
        &self,
        url: &str,
        accept: &str,
        registry: &str,
    ) -> Result<reqwest::Response, NpmError> {
        let mut request = yapi_ai::http::client()
            .get(url)
            .header(reqwest::header::ACCEPT, accept);
        if let Some(authorization) = self.authorization(url, registry)
            && let Ok(mut value) = reqwest::header::HeaderValue::from_str(&authorization)
        {
            value.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        let response = request
            .send()
            .await
            .map_err(|err| NpmError::Registry(format!("GET {url}: {err}")))?;
        if !response.status().is_success() {
            return Err(NpmError::Registry(format!(
                "GET {url}: {}",
                response.status()
            )));
        }
        Ok(response)
    }

    async fn packument(&mut self, name: &str) -> Result<&Value, NpmError> {
        if !self.packuments.contains_key(name) {
            let registry = self.registry(name);
            let url = format!("{registry}{}", name.replace('/', "%2f"));
            let body = self
                .get(&url, PACKUMENT_ACCEPT, &registry)
                .await?
                .bytes()
                .await
                .map_err(|err| NpmError::Registry(format!("GET {url}: {err}")))?;
            let packument: Value = serde_json::from_slice(&body)
                .map_err(|err| NpmError::Registry(format!("GET {url}: {err}")))?;
            self.packuments.insert(name.to_owned(), packument);
        }
        // Present: found or inserted above.
        Ok(&self.packuments[name])
    }

    /// The manifest of the version of `name` that `range` selects: a dist-tag,
    /// else the latest tag when it matches, else the highest match.
    async fn pick(&mut self, name: &str, range: &str) -> Result<Value, NpmError> {
        let packument = self.packument(name).await?;
        let no_versions = Map::new();
        let versions = packument["versions"].as_object().unwrap_or(&no_versions);
        let range = if range.is_empty() { "latest" } else { range };
        let no_match = || NpmError::NoMatch {
            name: name.to_owned(),
            range: range.to_owned(),
        };
        if let Some(tagged) = packument["dist-tags"][range].as_str() {
            return versions.get(tagged).cloned().ok_or_else(no_match);
        }
        if let Some(latest) = packument["dist-tags"]["latest"].as_str()
            && satisfies(latest, range)
            && let Some(manifest) = versions.get(latest)
        {
            return Ok(manifest.clone());
        }
        let alternatives = requirements(range).ok_or_else(no_match)?;
        versions
            .iter()
            .filter_map(|(version, manifest)| {
                Some((semver::Version::parse(version).ok()?, manifest))
            })
            .filter(|(version, _)| alternatives.iter().any(|req| req.matches(version)))
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, manifest)| manifest.clone())
            .ok_or_else(no_match)
    }

    async fn tarball(&self, manifest: &Value) -> Result<Vec<u8>, NpmError> {
        let id = format!("{}@{}", text(&manifest["name"]), text(&manifest["version"]));
        let url = manifest["dist"]["tarball"].as_str().unwrap_or_default();
        let registry = self.registry(&text(&manifest["name"]));
        let bytes = self
            .get(url, "*/*", &registry)
            .await?
            .bytes()
            .await
            .map_err(|err| NpmError::Registry(format!("GET {url}: {err}")))?
            .to_vec();
        if verify(&bytes, &manifest["dist"]) {
            Ok(bytes)
        } else {
            Err(NpmError::Integrity(id))
        }
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Checks `bytes` against the strongest algorithm of the registry's
/// `integrity` that yapi knows (sha512, then sha1), as npm's ssri does, and
/// against `shasum` when `integrity` has neither.
fn verify(bytes: &[u8], dist: &Value) -> bool {
    let integrity = dist["integrity"].as_str().unwrap_or_default();
    let hashes = |prefix: &str| -> Vec<&str> {
        integrity
            .split_whitespace()
            .filter_map(|entry| entry.strip_prefix(prefix))
            .collect()
    };
    let sha512 = hashes("sha512-");
    if !sha512.is_empty() {
        return sha512.contains(&STANDARD.encode(sha2::Sha512::digest(bytes)).as_str());
    }
    let sha1 = sha1::Sha1::digest(bytes);
    let sha1_hashes = hashes("sha1-");
    if !sha1_hashes.is_empty() {
        return sha1_hashes.contains(&STANDARD.encode(sha1).as_str());
    }
    dist["shasum"]
        .as_str()
        .is_some_and(|shasum| crate::time::hex(&sha1).eq_ignore_ascii_case(shasum))
}

/// Unpacks an npm tarball into `dir`, dropping each entry's first path
/// component (`package/`). Links and entries leaving `dir` are skipped.
fn unpack(bytes: &[u8], dir: &Path) -> Result<(), NpmError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let entries = archive.entries().map_err(io(dir))?;
    for entry in entries {
        let mut entry = entry.map_err(io(dir))?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let path = entry.path().map_err(io(dir))?.into_owned();
        let mut components = path.components();
        components.next();
        let relative: PathBuf = components.collect();
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            continue;
        }
        let target = dir.join(&relative);
        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(io(&target))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents).map_err(io(&target))?;
        std::fs::write(&target, contents).map_err(io(&target))?;
        #[cfg(unix)]
        if let Ok(mode) = entry.header().mode() {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(mode & 0o755 | 0o644),
            );
        }
    }
    Ok(())
}

/// The `package.json` of the package at `dir`, when it reads and parses. A
/// byte order mark is ignored, as npm ignores it.
fn read_manifest(dir: &Path) -> Option<Value> {
    yapi_types::json::parse(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()
}

/// The version of the package installed at `dir`, if any.
pub(crate) fn installed_version(dir: &Path) -> Option<String> {
    read_manifest(dir)?["version"].as_str().map(str::to_owned)
}

/// Installs into `root/node_modules`, hoisting dependencies there when Node's
/// resolution allows and nesting them otherwise.
struct Installer<'a> {
    npm: &'a mut Npm,
    root_modules: PathBuf,
}

impl Installer<'_> {
    /// Unpacks `manifest`'s package into `dir`, replacing what is there, then
    /// installs its dependencies.
    fn unpack_package<'b>(
        &'b mut self,
        dir: PathBuf,
        manifest: Value,
    ) -> BoxFuture<'b, Result<(), NpmError>> {
        Box::pin(async move {
            let bytes = self.npm.tarball(&manifest).await?;
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(io(&dir))?;
            }
            std::fs::create_dir_all(&dir).map_err(io(&dir))?;
            unpack(&bytes, &dir)?;
            self.dependencies(&dir, &manifest).await
        })
    }

    /// Installs the production and optional dependencies of the package in
    /// `dir`. Optional ones that fail or name a platform are skipped.
    async fn dependencies(&mut self, dir: &Path, manifest: &Value) -> Result<(), NpmError> {
        let none = Map::new();
        let required = manifest["dependencies"].as_object().unwrap_or(&none);
        let optional = manifest["optionalDependencies"]
            .as_object()
            .unwrap_or(&none);
        for (name, range) in required {
            if optional.contains_key(name) {
                continue;
            }
            self.place(dir, name, range.as_str().unwrap_or("*")).await?;
        }
        for (name, range) in optional {
            let _ = self.place(dir, name, range.as_str().unwrap_or("*")).await;
        }
        Ok(())
    }

    /// Makes dependency `name@range` of the package in `dependent` resolvable
    /// from it.
    fn place<'b>(
        &'b mut self,
        dependent: &'b Path,
        name: &'b str,
        range: &'b str,
    ) -> BoxFuture<'b, Result<(), NpmError>> {
        Box::pin(async move {
            if HOST_PACKAGES.contains(&name)
                || range.starts_with("file:")
                || range.starts_with("workspace:")
            {
                return Ok(());
            }
            // An alias, `npm:<package>@<range>`, installs that package under
            // `name`.
            let (package, range) = match range.strip_prefix("npm:") {
                Some(spec) => {
                    let (package, range) = super::source::split_npm_spec(spec);
                    (package, range.unwrap_or_else(|| "*".into()))
                }
                None => (name.to_owned(), range.to_owned()),
            };
            let nested = dependent.join("node_modules").join(name);
            if installed_version(&nested).is_some_and(|version| satisfies(&version, &range)) {
                return Ok(());
            }
            let hoisted = self.root_modules.join(name);
            let target = match installed_version(&hoisted) {
                Some(version) if satisfies(&version, &range) => return Ok(()),
                Some(_) => nested,
                None => hoisted,
            };
            let manifest = self.npm.pick(&package, &range).await?;
            if manifest.get("os").is_some() || manifest.get("cpu").is_some() {
                // Platform packages carry prebuilt binaries, never JS.
                return Ok(());
            }
            self.unpack_package(target, manifest).await
        })
    }
}

/// The `package.json` of an npm root, created as pi creates it.
fn root_manifest(root: &Path) -> Result<Map<String, Value>, NpmError> {
    std::fs::create_dir_all(root).map_err(io(root))?;
    let ignore = root.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n!.gitignore\n").map_err(io(&ignore))?;
    }
    let manifest = read_manifest(root)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_else(|| {
            let mut manifest = Map::new();
            manifest.insert("name".into(), json!("pi-extensions"));
            manifest.insert("private".into(), json!(true));
            manifest
        });
    Ok(manifest)
}

fn write_manifest(root: &Path, manifest: Map<String, Value>) -> Result<(), NpmError> {
    let path = root.join("package.json");
    let text =
        yapi_types::json::to_string_pretty(&Value::Object(manifest), "  ").unwrap_or_default();
    std::fs::write(&path, format!("{text}\n")).map_err(io(&path))
}

/// Installs `name` (at `range`, else the latest version) into the npm root
/// `root`, and records it in the root's `package.json`. Returns the version.
pub async fn install(
    npm: &mut Npm,
    root: &Path,
    name: &str,
    range: Option<&str>,
) -> Result<String, NpmError> {
    let mut manifest_file = root_manifest(root)?;
    let manifest = npm.pick(name, range.unwrap_or("latest")).await?;
    let version = text(&manifest["version"]);
    let modules = root.join("node_modules");
    let mut installer = Installer {
        npm,
        root_modules: modules.clone(),
    };
    installer
        .unpack_package(modules.join(name), manifest)
        .await?;
    let dependencies = manifest_file
        .entry("dependencies")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(dependencies) = dependencies.as_object_mut() {
        let spec = range.map_or_else(|| format!("^{version}"), str::to_owned);
        dependencies.insert(name.to_owned(), Value::String(spec));
    }
    write_manifest(root, manifest_file)?;
    Ok(version)
}

/// Installs the dependencies of the package in `dir` (a git checkout) into
/// its own `node_modules`.
pub async fn install_dependencies(npm: &mut Npm, dir: &Path) -> Result<(), NpmError> {
    let Some(manifest) = read_manifest(dir) else {
        return Ok(());
    };
    let mut installer = Installer {
        npm,
        root_modules: dir.join("node_modules"),
    };
    installer.dependencies(dir, &manifest).await
}

/// Removes `name` from the npm root `root`, then the packages nothing
/// depends on any more.
pub fn uninstall(root: &Path, name: &str) -> Result<(), NpmError> {
    if !root.exists() {
        return Ok(());
    }
    let mut manifest = root_manifest(root)?;
    if let Some(dependencies) = manifest
        .get_mut("dependencies")
        .and_then(Value::as_object_mut)
    {
        dependencies.shift_remove(name);
    }
    let dir = root.join("node_modules").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(io(&dir))?;
    }
    let roots: Vec<String> = manifest
        .get("dependencies")
        .and_then(Value::as_object)
        .map(|dependencies| dependencies.keys().cloned().collect())
        .unwrap_or_default();
    write_manifest(root, manifest)?;
    prune(&root.join("node_modules"), &roots);
    Ok(())
}

/// Removes top-level packages in `modules` that no package in `roots`
/// reaches through Node's resolution.
fn prune(modules: &Path, roots: &[String]) {
    let mut reached: Vec<PathBuf> = Vec::new();
    let mut queue: Vec<PathBuf> = roots.iter().map(|name| modules.join(name)).collect();
    while let Some(dir) = queue.pop() {
        if reached.contains(&dir) || !dir.exists() {
            continue;
        }
        reached.push(dir.clone());
        let manifest = read_manifest(&dir).unwrap_or(Value::Null);
        for key in ["dependencies", "optionalDependencies"] {
            for name in manifest[key]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, _)| name)
            {
                let nested = dir.join("node_modules").join(name);
                queue.push(if nested.exists() {
                    nested
                } else {
                    modules.join(name)
                });
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(modules) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let candidates: Vec<PathBuf> = if name.starts_with('@') {
            std::fs::read_dir(&path)
                .map(|scoped| scoped.flatten().map(|entry| entry.path()).collect())
                .unwrap_or_default()
        } else if name.starts_with('.') {
            Vec::new()
        } else {
            vec![path]
        };
        for candidate in candidates {
            if !reached.contains(&candidate) {
                let _ = std::fs::remove_dir_all(&candidate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_npm_ranges() {
        assert!(satisfies("1.4.0", "^1.2.3"));
        assert!(!satisfies("2.0.0", "^1.2.3"));
        assert!(satisfies("1.2.9", "~1.2.0"));
        assert!(satisfies("1.5.0", ">=1.2 <2"));
        assert!(satisfies("3.0.0", "^1 || ^3"));
        assert!(satisfies("1.9.9", "1.x"));
        assert!(satisfies("0.1.0", "*"));
        assert!(satisfies("1.3.0", "1.2.0 - 1.4.0"));
        assert!(!satisfies("1.0.0-beta.1", "^1.0.0"));
        // A bare version is exact, and a partial one fixes what it names.
        assert!(satisfies("0.35.0", "0.35.0"));
        assert!(!satisfies("0.35.1", "0.35.0"));
        assert!(!satisfies("1.3.0", "1.2.3"));
        assert!(satisfies("1.2.9", "1.2"));
        assert!(!satisfies("1.3.0", "1.2"));
        assert!(satisfies("1.9.0", "1"));
        assert!(!satisfies("2.0.0", "1"));
        assert!(satisfies("1.2.3", "1.2.3 || 2"));
        assert!(!satisfies("1.2.4", "1.2.3 || 2"));
        assert!(satisfies("1.2.9", "1.2.x"));
        assert!(!satisfies("1.3.0", "1.2.x"));
    }

    #[test]
    fn reads_npmrc_settings_with_environment_references() {
        let env = |name: &str| (name == "TOKEN").then(|| "secret".to_owned());
        let config = parse_config(
            "; comment\n\
             # comment\n\
             registry = https://registry.example/ \n\
             @acme:registry=\"https://npm.acme.test/\"\n\
             //npm.acme.test/:_authToken=${TOKEN}\n\
             //other.test/:_authToken=${MISSING}\n\
             //optional.test/:_authToken=${MISSING?}\n\
             //token.test/:_auth=dXNlcjpwYXNz ; comment\n\
             [section]\n\
             ignored=1\n",
            env,
        );
        let mut entries: Vec<_> = config.iter().collect();
        entries.sort();
        assert_eq!(
            entries,
            [
                (
                    &"//npm.acme.test/:_authToken".to_owned(),
                    &"secret".to_owned()
                ),
                (&"//other.test/:_authToken".into(), &"${MISSING}".into()),
                (&"//token.test/:_auth".into(), &"dXNlcjpwYXNz".into()),
                (&"@acme:registry".into(), &"https://npm.acme.test/".into()),
                (&"registry".into(), &"https://registry.example/".into()),
            ]
        );
        assert_eq!(
            expand("a${TOKEN}b${${TOKEN}}${}", env),
            "asecretb${secret}${}"
        );
    }

    #[test]
    fn sends_credentials_only_where_their_key_covers_the_url() {
        let npm = Npm::new(HashMap::from(
            [
                ("@acme:registry", "https://npm.acme.test"),
                ("//npm.acme.test/:_authToken", "acme"),
                ("//npm.acme.test/private/:_authToken", "private"),
                ("//basic.test/:username", "user"),
                ("//basic.test/:_password", "cGFzcw=="),
                ("//legacy.test:8080/:_auth", "dXNlcjpwYXNz"),
                ("//gitlab.test/api/v4/packages/npm/:_authToken", "gitlab"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
        ));
        assert_eq!(npm.registry("@acme/pkg"), "https://npm.acme.test/");
        assert_eq!(npm.registry("pkg"), "https://registry.npmjs.org/");
        let auth = |url: &str, registry: &str| npm.authorization(url, registry);
        let acme = "https://npm.acme.test/";
        assert_eq!(
            auth("https://npm.acme.test/@acme%2fpkg", acme).unwrap(),
            "Bearer acme"
        );
        assert_eq!(
            auth("https://npm.acme.test/private/pkg.tgz", acme).unwrap(),
            "Bearer private"
        );
        assert_eq!(
            auth("https://npm.acme.test/privateer/pkg.tgz", acme).unwrap(),
            "Bearer acme"
        );
        assert_eq!(
            auth("https://basic.test/pkg", "https://basic.test/").unwrap(),
            format!("Basic {}", STANDARD.encode("user:pass"))
        );
        assert_eq!(
            auth("http://legacy.test:8080/pkg", acme).unwrap(),
            "Basic dXNlcjpwYXNz"
        );
        // A tarball elsewhere on the registry's host gets the registry's.
        let gitlab = "https://gitlab.test/api/v4/packages/npm/";
        assert_eq!(
            auth(
                "https://gitlab.test/api/v4/projects/7/packages/npm/pkg.tgz",
                gitlab
            )
            .unwrap(),
            "Bearer gitlab"
        );
        // Other hosts and ports get nothing, tarballs included.
        assert_eq!(auth("http://legacy.test/pkg", acme), None);
        assert_eq!(auth("https://npm.acme.test.evil/pkg", acme), None);
        assert_eq!(auth("https://cdn.acme.test/pkg.tgz", acme), None);
        assert_eq!(
            auth(
                "https://registry.npmjs.org/pkg",
                "https://registry.npmjs.org/"
            ),
            None
        );
    }

    #[test]
    fn reads_manifests_with_a_byte_order_mark() {
        let dir = std::env::temp_dir().join(format!("yapi-npm-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "\u{feff}{\"version\": \"1.2.3\"}").unwrap();
        assert_eq!(installed_version(&dir).as_deref(), Some("1.2.3"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ranges_may_prefix_versions_with_v() {
        assert!(satisfies("1.2.3", "v1.2.3"));
        assert!(!satisfies("1.2.4", "v1.2.3"));
        assert!(satisfies("1.4.0", "^v1.2.3"));
        assert!(satisfies("1.2.9", "~v1.2.0"));
        assert!(satisfies("1.5.0", ">=v1.2 <v2"));
        assert!(satisfies("1.5.0", ">= v1.2 < v2"));
        assert!(satisfies("1.3.0", "v1.2.0 - v1.4.0"));
        assert!(satisfies("2.0.0", "v1 || v2.x"));
    }

    #[test]
    fn verifies_the_strongest_known_integrity_then_shasum() {
        let bytes = b"tarball";
        let sha512 = format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(bytes)));
        let sha1 = format!("sha1-{}", STANDARD.encode(sha1::Sha1::digest(bytes)));
        let shasum = crate::time::hex(&sha1::Sha1::digest(bytes));
        let wrong512 = format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(b"other")));
        let check = |dist: Value| verify(bytes, &dist);
        assert!(check(json!({"integrity": sha512})));
        assert!(check(json!({"integrity": sha1})));
        assert!(!check(json!({"integrity": "sha1-AAAA"})));
        // sha512 decides when present, as the strongest algorithm.
        assert!(!check(json!({"integrity": format!("{wrong512} {sha1}")})));
        assert!(check(json!({"integrity": format!("{wrong512} {sha512}")})));
        // An integrity without a known algorithm falls back to `shasum`.
        assert!(check(json!({"integrity": "sha384-AAAA", "shasum": shasum})));
        assert!(check(json!({"shasum": shasum.to_uppercase()})));
        assert!(!check(json!({"integrity": "sha384-AAAA"})));
        assert!(!check(json!({})));
    }
}
