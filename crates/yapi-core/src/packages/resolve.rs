//! Every resource that settings, packages and the agent and project
//! directories provide, each enabled or not, in load precedence order.
//!
//! Port of `DefaultPackageManager.resolve` and its pattern rules in
//! `core/package-manager.ts` in pi `v1.0.0`. yapi also loads `.wasm` native
//! extensions wherever pi loads `.ts` and `.js` ones.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use yapi_types::rpc::SourceInfo;
use yapi_types::settings::FilteredPackage;

use crate::glob;
use crate::tools::path::{relative, resolve_lexically};

/// A kind of resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceType {
    /// Extension entry files.
    Extensions,
    /// Skill files.
    Skills,
    /// Prompt template files.
    Prompts,
    /// Theme files.
    Themes,
}

impl ResourceType {
    /// Every kind, in pi's order.
    pub const ALL: [ResourceType; 4] = [
        ResourceType::Extensions,
        ResourceType::Skills,
        ResourceType::Prompts,
        ResourceType::Themes,
    ];

    /// The settings and manifest key.
    pub fn key(self) -> &'static str {
        match self {
            ResourceType::Extensions => "extensions",
            ResourceType::Skills => "skills",
            ResourceType::Prompts => "prompts",
            ResourceType::Themes => "themes",
        }
    }

    fn matches_file(self, name: &str) -> bool {
        match self {
            ResourceType::Extensions => is_extension_file(name),
            ResourceType::Skills | ResourceType::Prompts => name.ends_with(".md"),
            ResourceType::Themes => name.ends_with(".json"),
        }
    }
}

/// pi's extension files, and yapi's native extensions.
fn is_extension_file(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".js") || name.ends_with(".wasm")
}

/// A resource and whether it loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Its path and where it comes from.
    pub info: SourceInfo,
    /// Whether settings leave it on.
    pub enabled: bool,
}

/// Resolved resources by kind; pi's `ResolvedPaths`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    /// Extensions.
    pub extensions: Vec<Resolved>,
    /// Skills.
    pub skills: Vec<Resolved>,
    /// Prompt templates.
    pub prompts: Vec<Resolved>,
    /// Themes.
    pub themes: Vec<Resolved>,
}

impl ResolvedPaths {
    /// The resources of `kind`.
    pub fn of(&self, kind: ResourceType) -> &[Resolved] {
        match kind {
            ResourceType::Extensions => &self.extensions,
            ResourceType::Skills => &self.skills,
            ResourceType::Prompts => &self.prompts,
            ResourceType::Themes => &self.themes,
        }
    }

    fn of_mut(&mut self, kind: ResourceType) -> &mut Vec<Resolved> {
        match kind {
            ResourceType::Extensions => &mut self.extensions,
            ResourceType::Skills => &mut self.skills,
            ResourceType::Prompts => &mut self.prompts,
            ResourceType::Themes => &mut self.themes,
        }
    }

    /// Adds the resource at `path` with `metadata` unless it is listed.
    fn add(&mut self, kind: ResourceType, path: &Path, metadata: &SourceInfo, enabled: bool) {
        let path = path.to_string_lossy().into_owned();
        if path.is_empty() {
            return;
        }
        let list = self.of_mut(kind);
        if list.iter().any(|resource| resource.info.path == path) {
            return;
        }
        list.push(Resolved {
            info: SourceInfo {
                path,
                ..metadata.clone()
            },
            enabled,
        });
    }

    /// The enabled resources of `kind`, in order.
    pub fn enabled(&self, kind: ResourceType) -> impl Iterator<Item = &SourceInfo> {
        self.of(kind)
            .iter()
            .filter(|resource| resource.enabled)
            .map(|resource| &resource.info)
    }
}

/// A package to collect from, after pi's deduplication.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageInput {
    /// The source as written in settings.
    pub source: String,
    /// `user`, `project` or `temporary`.
    pub scope: String,
    /// Where it is installed: a directory, or a file for a local file.
    pub root: PathBuf,
    /// The settings entry's filters, when it is an object.
    pub filter: Option<FilteredPackage>,
}

/// What [`resolve`] reads.
#[derive(Clone, Debug, Default)]
pub struct ResolveInput<'a> {
    /// The working directory.
    pub cwd: PathBuf,
    /// The agent directory.
    pub agent_dir: PathBuf,
    /// The project's directory of yapi files, `<cwd>/.yapi`.
    pub project_dir: PathBuf,
    /// The home directory, for `~/.agents/skills`.
    pub home: PathBuf,
    /// Whether the project's own resources load.
    pub project_trusted: bool,
    /// Installed packages, project entries first.
    pub packages: Vec<PackageInput>,
    /// The user's `extensions`, `skills`, `prompts` and `themes` settings.
    pub user: [&'a [String]; 4],
    /// The project's.
    pub project: [&'a [String]; 4],
    /// Built-in extension names.
    pub builtins: &'a [&'a str],
}

/// The path prefix naming a built-in extension.
pub const BUILTIN_PREFIX: &str = "builtin:";

fn to_posix(path: &str) -> String {
    path.replace('\\', "/")
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// pi's `matchesAnyPattern`: a glob against the path relative to `base`,
/// the file name or the whole path; for `SKILL.md`, its directory too.
fn matches_any_pattern(file: &Path, patterns: &[String], base: &Path) -> bool {
    let rel = relative(base, file);
    let name = base_name(file);
    let whole = to_posix(&file.to_string_lossy());
    let parent = (name == "SKILL.md").then(|| file.parent()).flatten();
    patterns.iter().any(|pattern| {
        let pattern = to_posix(pattern);
        // minimatch keeps a leading `./`, which no relative path, name or
        // absolute path has.
        if pattern.starts_with("./") {
            return false;
        }
        if glob::matches(&pattern, &rel)
            || glob::matches(&pattern, &name)
            || glob::matches(&pattern, &whole)
        {
            return true;
        }
        parent.is_some_and(|parent| {
            glob::matches(&pattern, &relative(base, parent))
                || glob::matches(&pattern, &base_name(parent))
                || glob::matches(&pattern, &to_posix(&parent.to_string_lossy()))
        })
    })
}

/// pi's `matchesAnyExactPattern`: the path relative to `base` or the whole
/// path, without a leading `./`; for `SKILL.md`, its directory too.
fn matches_any_exact_pattern(file: &Path, patterns: &[String], base: &Path) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let rel = relative(base, file);
    let whole = to_posix(&file.to_string_lossy());
    let parent = (base_name(file) == "SKILL.md")
        .then(|| file.parent())
        .flatten();
    patterns.iter().any(|pattern| {
        let pattern = pattern
            .strip_prefix("./")
            .or_else(|| pattern.strip_prefix(".\\"))
            .unwrap_or(pattern);
        let pattern = to_posix(pattern);
        if pattern == rel || pattern == whole {
            return true;
        }
        parent.is_some_and(|parent| {
            pattern == relative(base, parent) || pattern == to_posix(&parent.to_string_lossy())
        })
    })
}

fn is_override(entry: &str) -> bool {
    entry.starts_with(['!', '+', '-'])
}

fn is_pattern(entry: &str) -> bool {
    is_override(entry) || entry.contains(['*', '?'])
}

fn has_glob(entry: &str) -> bool {
    entry.contains(['*', '?'])
}

fn stripped<'a>(entries: impl Iterator<Item = &'a String>, sign: char) -> Vec<String> {
    entries
        .filter_map(|entry| entry.strip_prefix(sign).map(str::to_owned))
        .collect()
}

/// pi's `isEnabledByOverrides`: `!` globs disable, then `+` paths enable,
/// then `-` paths disable.
pub fn is_enabled_by_overrides(file: &Path, entries: &[String], base: &Path) -> bool {
    let excludes = stripped(entries.iter(), '!');
    let force_includes = stripped(entries.iter(), '+');
    let force_excludes = stripped(entries.iter(), '-');
    let mut enabled = true;
    if !excludes.is_empty() && matches_any_pattern(file, &excludes, base) {
        enabled = false;
    }
    if matches_any_exact_pattern(file, &force_includes, base) {
        enabled = true;
    }
    if matches_any_exact_pattern(file, &force_excludes, base) {
        enabled = false;
    }
    enabled
}

/// pi's `applyPatterns`: plain globs select (all when there are none), `!`
/// globs remove, `+` paths add back and `-` paths remove.
pub fn apply_patterns(all: &[PathBuf], patterns: &[String], base: &Path) -> HashSet<PathBuf> {
    let includes: Vec<String> = patterns
        .iter()
        .filter(|pattern| !is_override(pattern))
        .cloned()
        .collect();
    let excludes = stripped(patterns.iter(), '!');
    let force_includes = stripped(patterns.iter(), '+');
    let force_excludes = stripped(patterns.iter(), '-');
    let mut result: Vec<PathBuf> = if includes.is_empty() {
        all.to_vec()
    } else {
        all.iter()
            .filter(|file| matches_any_pattern(file, &includes, base))
            .cloned()
            .collect()
    };
    if !excludes.is_empty() {
        result.retain(|file| !matches_any_pattern(file, &excludes, base));
    }
    for file in all {
        if !result.contains(file) && matches_any_exact_pattern(file, &force_includes, base) {
            result.push(file.clone());
        }
    }
    if !force_excludes.is_empty() {
        result.retain(|file| !matches_any_exact_pattern(file, &force_excludes, base));
    }
    result.into_iter().collect()
}

/// pi's `applyAutoloadDisabledPatterns`: each pattern turns the files it
/// names on (`+`, plain) or off (`-`, `!`), the last one deciding.
fn apply_autoload_disabled_patterns(
    all: &[PathBuf],
    patterns: &[String],
    base: &Path,
) -> Vec<(PathBuf, bool)> {
    let mut result: Vec<(PathBuf, bool)> = Vec::new();
    for pattern in patterns {
        let target = if is_override(pattern) {
            pattern[1..].to_owned()
        } else {
            pattern.clone()
        };
        let enabled = !pattern.starts_with(['-', '!']);
        let exact = pattern.starts_with(['+', '-']);
        for file in all {
            let found = if exact {
                matches_any_exact_pattern(file, std::slice::from_ref(&target), base)
            } else {
                matches_any_pattern(file, std::slice::from_ref(&target), base)
            };
            if found {
                match result.iter_mut().find(|(path, _)| path == file) {
                    Some(entry) => entry.1 = enabled,
                    None => result.push((file.clone(), enabled)),
                }
            }
        }
    }
    result
}

/// The rules of `.gitignore`, `.ignore` and `.fdignore` files met while
/// walking a directory; a subset of the `ignore` package pi uses.
#[derive(Clone, Debug, Default)]
struct Ignore {
    rules: Vec<(String, bool)>,
}

impl Ignore {
    /// pi's `addIgnoreRules`: the rules in `dir`, prefixed with its path
    /// relative to `root`.
    fn add_dir(&mut self, dir: &Path, root: &Path) {
        let rel = relative(root, dir);
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        for name in [".gitignore", ".ignore", ".fdignore"] {
            let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
                continue;
            };
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("\\#")) {
                    continue;
                }
                let (mut pattern, negated) = match line.strip_prefix('!') {
                    Some(rest) => (rest, true),
                    None => (
                        line.strip_prefix('\\')
                            .filter(|rest| rest.starts_with('!'))
                            .unwrap_or(line),
                        false,
                    ),
                };
                pattern = pattern.strip_prefix('/').unwrap_or(pattern);
                self.rules
                    .push((format!("{prefix}{}", pattern.trim_end()), negated));
            }
        }
    }

    /// Whether `path`, relative to the walk's root and ending in `/` for a
    /// directory, is ignored.
    fn ignores(&self, path: &str) -> bool {
        let dir = path.ends_with('/');
        let path = path.trim_end_matches('/');
        let mut ignored = false;
        for (pattern, negated) in &self.rules {
            if rule_matches(pattern, path, dir) {
                ignored = !negated;
            }
        }
        ignored
    }
}

/// One rule against a relative path. A rule with a `/` is anchored to the
/// root; one without matches a name at any depth. Walks skip dot entries
/// before asking, so wildcards need not match them.
fn rule_matches(pattern: &str, path: &str, dir: bool) -> bool {
    let (pattern, dir_only) = match pattern.strip_suffix('/') {
        Some(pattern) => (pattern, true),
        None => (pattern, false),
    };
    if dir_only && !dir {
        return false;
    }
    if pattern.contains('/') {
        glob::matches(pattern, path)
    } else {
        glob::matches(pattern, path.rsplit('/').next().unwrap_or(path))
    }
}

struct Entry {
    path: PathBuf,
    is_dir: bool,
    is_file: bool,
    name: String,
}

/// A directory's entries, sorted by name, following symbolic links.
fn read_dir(dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = std::fs::metadata(&path).ok()?;
            Some(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: metadata.is_dir(),
                is_file: metadata.is_file(),
                path,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// pi's `collectFiles`: files of `kind` under `dir`, recursively, skipping
/// dot entries, `node_modules` and ignored paths.
fn collect_files(dir: &Path, kind: ResourceType, ignore: &mut Ignore, root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.exists() {
        return files;
    }
    ignore.add_dir(dir, root);
    for entry in read_dir(dir) {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let rel = relative(root, &entry.path);
        let probe = if entry.is_dir { format!("{rel}/") } else { rel };
        if ignore.ignores(&probe) {
            continue;
        }
        if entry.is_dir {
            files.extend(collect_files(&entry.path, kind, ignore, root));
        } else if entry.is_file && kind.matches_file(&entry.name) {
            files.push(entry.path);
        }
    }
    files
}

/// Where skills are discovered: pi's own directories take `.md` files at the
/// top, `.agents` directories only below it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SkillMode {
    Pi,
    Agents,
}

/// pi's `collectSkillEntries`: a directory with a `SKILL.md` is one skill;
/// otherwise its `.md` files (by mode) and the skills of its directories.
fn collect_skill_entries(
    dir: &Path,
    mode: SkillMode,
    ignore: &mut Ignore,
    root: &Path,
) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if !dir.exists() {
        return entries;
    }
    ignore.add_dir(dir, root);
    let listing = read_dir(dir);
    if let Some(skill) = listing.iter().find(|entry| entry.name == "SKILL.md")
        && skill.is_file
        && !ignore.ignores(&relative(root, &skill.path))
    {
        entries.push(skill.path.clone());
        return entries;
    }
    for entry in listing {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let rel = relative(root, &entry.path);
        let markdown = entry.is_file
            && entry.name.ends_with(".md")
            && !ignore.ignores(&rel)
            && ((mode == SkillMode::Pi && dir == root)
                || (mode == SkillMode::Agents && dir != root));
        if markdown {
            entries.push(entry.path);
            continue;
        }
        if !entry.is_dir || ignore.ignores(&format!("{rel}/")) {
            continue;
        }
        entries.extend(collect_skill_entries(&entry.path, mode, ignore, root));
    }
    entries
}

/// pi's `collectAutoPromptEntries` and `collectAutoThemeEntries`: the files
/// of `kind` directly in `dir`.
fn collect_top_files(dir: &Path, kind: ResourceType) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut ignore = Ignore::default();
    ignore.add_dir(dir, dir);
    read_dir(dir)
        .into_iter()
        .filter(|entry| !entry.name.starts_with('.') && entry.name != "node_modules")
        .filter(|entry| !ignore.ignores(&relative(dir, &entry.path)))
        .filter(|entry| entry.is_file && kind.matches_file(&entry.name))
        .map(|entry| entry.path)
        .collect()
}

/// pi's `collectAutoExtensionEntries`: a directory's own entry points, else
/// its extension files and the entry points of its directories.
fn collect_auto_extension_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    if let Some(entries) = crate::extensions::discovery::entries(dir) {
        return entries;
    }
    let mut ignore = Ignore::default();
    ignore.add_dir(dir, dir);
    let mut entries = Vec::new();
    for entry in read_dir(dir) {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let rel = relative(dir, &entry.path);
        let probe = if entry.is_dir { format!("{rel}/") } else { rel };
        if ignore.ignores(&probe) {
            continue;
        }
        if entry.is_file && is_extension_file(&entry.name) {
            entries.push(entry.path);
        } else if entry.is_dir {
            entries.extend(crate::extensions::discovery::entries(&entry.path).unwrap_or_default());
        }
    }
    entries
}

/// pi's `collectResourceFiles`.
fn collect_resource_files(dir: &Path, kind: ResourceType) -> Vec<PathBuf> {
    match kind {
        ResourceType::Skills => {
            collect_skill_entries(dir, SkillMode::Pi, &mut Ignore::default(), dir)
        }
        ResourceType::Extensions => collect_auto_extension_entries(dir),
        _ => collect_files(dir, kind, &mut Ignore::default(), dir),
    }
}

/// pi's `collectFilesFromPaths`: files as they are, directories collected.
fn collect_files_from_paths(paths: &[PathBuf], kind: ResourceType) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        if metadata.is_file() {
            files.push(path.clone());
        } else if metadata.is_dir() {
            files.extend(collect_resource_files(path, kind));
        }
    }
    files
}

/// pi's `readPiManifest`: the `yapi` key of a package's `package.json`, else
/// its `pi` key. A list is `None` when absent or not all strings.
#[derive(Default)]
pub(crate) struct Manifest {
    lists: [Option<Vec<String>>; 4],
}

impl Manifest {
    /// The manifest of the package at `root`; `None` when its `package.json`
    /// is unreadable or declares neither key.
    pub(crate) fn read(root: &Path) -> Option<Manifest> {
        let text = std::fs::read_to_string(root.join("package.json")).ok()?;
        let package: serde_json::Value = yapi_types::json::parse(&text).ok()?;
        let manifest = ["yapi", "pi"]
            .iter()
            .find_map(|key| package.get(*key).filter(|value| value.is_object()))?;
        let list = |kind: ResourceType| {
            let entries = manifest[kind.key()].as_array()?;
            entries
                .iter()
                .map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        };
        Some(Manifest {
            lists: ResourceType::ALL.map(list),
        })
    }

    /// The entries of `kind`, when listed.
    pub(crate) fn get(&self, kind: ResourceType) -> Option<&Vec<String>> {
        self.lists[kind as usize].as_ref()
    }
}

/// pi's `expandPackageGlob`: matches under `root` with no dot segment,
/// sorted.
fn expand_package_glob(pattern: &str, root: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in read_dir(dir) {
            let rel = relative(root, &entry.path);
            out.push(rel);
            if entry.is_dir && entry.name != "node_modules" && !entry.name.starts_with('.') {
                walk(root, &entry.path, out);
            }
        }
    }
    let mut all = Vec::new();
    walk(root, root, &mut all);
    let mut found: Vec<PathBuf> = all
        .into_iter()
        .filter(|rel| rel.split('/').all(|segment| !segment.starts_with('.')))
        .filter(|rel| glob::matches(pattern, rel))
        .map(|rel| root.join(rel))
        .collect();
    found.sort();
    found
}

/// pi's `collectFilesFromManifestEntries`.
fn manifest_files(entries: &[String], root: &Path, kind: ResourceType) -> Vec<PathBuf> {
    let resolved: Vec<PathBuf> = entries
        .iter()
        .filter(|entry| !is_override(entry))
        .flat_map(|entry| {
            if has_glob(entry) {
                expand_package_glob(entry, root)
            } else {
                vec![resolve_lexically(root, Path::new(entry))]
            }
        })
        .collect();
    collect_files_from_paths(&resolved, kind)
}

fn metadata(source: &str, scope: &str, origin: &str, base_dir: Option<&Path>) -> SourceInfo {
    SourceInfo {
        path: String::new(),
        source: source.to_owned(),
        scope: scope.to_owned(),
        origin: origin.to_owned(),
        base_dir: base_dir.map(|dir| dir.to_string_lossy().into_owned()),
    }
}

/// pi's `collectPackageResources`; whether the package provided anything.
fn collect_package_resources(
    root: &Path,
    acc: &mut ResolvedPaths,
    filter: Option<&FilteredPackage>,
    meta: &SourceInfo,
) -> bool {
    let manifest = Manifest::read(root);
    if let Some(filter) = filter {
        for kind in ResourceType::ALL {
            let patterns = match kind {
                ResourceType::Extensions => &filter.extensions,
                ResourceType::Skills => &filter.skills,
                ResourceType::Prompts => &filter.prompts,
                ResourceType::Themes => &filter.themes,
            };
            if filter.autoload == Some(false) {
                let patterns = patterns.clone().unwrap_or_default();
                if patterns.is_empty() {
                    continue;
                }
                let all = package_files(root, manifest.as_ref(), kind);
                for (file, enabled) in apply_autoload_disabled_patterns(&all, &patterns, root) {
                    acc.add(kind, &file, meta, enabled);
                }
            } else if let Some(patterns) = patterns {
                let all = package_files(root, manifest.as_ref(), kind);
                let enabled = if patterns.is_empty() {
                    HashSet::new()
                } else {
                    apply_patterns(&all, patterns, root)
                };
                for file in &all {
                    acc.add(kind, file, meta, enabled.contains(file));
                }
            } else {
                default_resources(root, manifest.as_ref(), kind, acc, meta);
            }
        }
        return true;
    }
    if let Some(manifest) = &manifest {
        for kind in ResourceType::ALL {
            if let Some(entries) = manifest.get(kind) {
                manifest_entries(entries, root, kind, acc, meta);
            }
        }
        return true;
    }
    let mut any = false;
    for kind in ResourceType::ALL {
        let dir = root.join(kind.key());
        if dir.exists() {
            for file in collect_resource_files(&dir, kind) {
                acc.add(kind, &file, meta, true);
            }
            any = true;
        }
    }
    any
}

/// pi's `collectDefaultResources`.
fn default_resources(
    root: &Path,
    manifest: Option<&Manifest>,
    kind: ResourceType,
    acc: &mut ResolvedPaths,
    meta: &SourceInfo,
) {
    if let Some(entries) = manifest.and_then(|manifest| manifest.get(kind)) {
        manifest_entries(entries, root, kind, acc, meta);
        return;
    }
    let dir = root.join(kind.key());
    if dir.exists() {
        for file in collect_resource_files(&dir, kind) {
            acc.add(kind, &file, meta, true);
        }
    }
}

/// pi's `collectManifestFiles`: the files the manifest's entries select,
/// or the conventional directory's.
fn package_files(root: &Path, manifest: Option<&Manifest>, kind: ResourceType) -> Vec<PathBuf> {
    if let Some(entries) = manifest
        .and_then(|manifest| manifest.get(kind))
        .filter(|entries| !entries.is_empty())
    {
        let all = manifest_files(entries, root, kind);
        let patterns: Vec<String> = entries
            .iter()
            .filter(|entry| is_override(entry))
            .cloned()
            .collect();
        if patterns.is_empty() {
            return all;
        }
        let enabled = apply_patterns(&all, &patterns, root);
        return all
            .into_iter()
            .filter(|file| enabled.contains(file))
            .collect();
    }
    let dir = root.join(kind.key());
    if dir.exists() {
        collect_resource_files(&dir, kind)
    } else {
        Vec::new()
    }
}

/// pi's `addManifestEntries`: the enabled files only.
fn manifest_entries(
    entries: &[String],
    root: &Path,
    kind: ResourceType,
    acc: &mut ResolvedPaths,
    meta: &SourceInfo,
) {
    let all = manifest_files(entries, root, kind);
    let patterns: Vec<String> = entries
        .iter()
        .filter(|entry| is_override(entry))
        .cloned()
        .collect();
    let enabled = apply_patterns(&all, &patterns, root);
    for file in &all {
        if enabled.contains(file) {
            acc.add(kind, file, meta, true);
        }
    }
}

/// pi's `resolveLocalEntries`: settings entries as paths, with their
/// patterns deciding which files are on.
fn local_entries(
    entries: &[String],
    kind: ResourceType,
    acc: &mut ResolvedPaths,
    meta: &SourceInfo,
    base: &Path,
) {
    if entries.is_empty() {
        return;
    }
    let (patterns, plain): (Vec<String>, Vec<String>) =
        entries.iter().cloned().partition(|entry| is_pattern(entry));
    let resolved: Vec<PathBuf> = plain
        .iter()
        .map(|entry| resolve_lexically(base, Path::new(&crate::tools::path::expand(entry.trim()))))
        .collect();
    let all = collect_files_from_paths(&resolved, kind);
    let enabled = apply_patterns(&all, &patterns, base);
    for file in &all {
        acc.add(kind, file, meta, enabled.contains(file));
    }
}

/// pi's `collectAncestorAgentsSkillDirs`: `.agents/skills` in `start` and
/// each parent up to the repository root, or the filesystem root.
fn ancestor_agents_skill_dirs(start: &Path) -> Vec<PathBuf> {
    let repo = start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf);
    let mut dirs = Vec::new();
    for dir in start.ancestors() {
        dirs.push(dir.join(".agents").join("skills"));
        if repo.as_deref() == Some(dir) {
            break;
        }
    }
    dirs
}

/// pi's `addAutoDiscoveredResources`: the project's resources when it is
/// trusted, then the user's; for each scope its extensions, skills, `.agents`
/// skills, prompts and themes.
fn auto_discovered(input: &ResolveInput<'_>, acc: &mut ResolvedPaths) {
    let user_agents = input.home.join(".agents").join("skills");
    let project_agents: Vec<PathBuf> = if input.project_trusted {
        ancestor_agents_skill_dirs(&input.cwd)
            .into_iter()
            .filter(|dir| *dir != user_agents)
            .collect()
    } else {
        Vec::new()
    };
    let scopes = [
        (
            "project",
            &input.project_dir,
            input.project,
            input.project_trusted,
            project_agents,
        ),
        (
            "user",
            &input.agent_dir,
            input.user,
            true,
            vec![user_agents],
        ),
    ];
    for (scope, base, overrides, own, agents) in scopes {
        let mut add = |kind: ResourceType, paths: Vec<PathBuf>, base: &Path| {
            let meta = metadata("auto", scope, "top-level", Some(base));
            for path in paths {
                let enabled = is_enabled_by_overrides(&path, overrides[kind as usize], base);
                acc.add(kind, &path, &meta, enabled);
            }
        };
        for kind in ResourceType::ALL {
            let dir = base.join(kind.key());
            if own {
                let paths = match kind {
                    ResourceType::Extensions => collect_auto_extension_entries(&dir),
                    ResourceType::Skills => {
                        collect_skill_entries(&dir, SkillMode::Pi, &mut Ignore::default(), &dir)
                    }
                    _ => collect_top_files(&dir, kind),
                };
                add(kind, paths, base);
            }
            if kind == ResourceType::Skills {
                for dir in &agents {
                    let paths =
                        collect_skill_entries(dir, SkillMode::Agents, &mut Ignore::default(), dir);
                    add(kind, paths, dir.parent().unwrap_or(dir));
                }
            }
        }
    }
}

/// pi's `resourcePrecedenceRank`.
fn precedence(info: &SourceInfo) -> u8 {
    if info.source == "builtin" {
        return 5;
    }
    if info.origin == "package" {
        return 4;
    }
    let scope = if info.scope == "project" { 0 } else { 2 };
    scope + u8::from(info.source != "local")
}

/// One package's resources, installed at `root`: a file is one extension,
/// and a folder gives what its manifest or resource folders list. pi takes a
/// folder with neither as one extension only when it is a `local` path.
fn collect_package(
    acc: &mut ResolvedPaths,
    root: &Path,
    filter: Option<&FilteredPackage>,
    local: bool,
    mut meta: SourceInfo,
) {
    if root.is_file() {
        meta.base_dir = root.parent().map(|dir| dir.to_string_lossy().into_owned());
        acc.add(ResourceType::Extensions, root, &meta, true);
        return;
    }
    if !root.is_dir() {
        return;
    }
    meta.base_dir = Some(root.to_string_lossy().into_owned());
    if !collect_package_resources(root, acc, filter, &meta) && local {
        for entry in crate::extensions::discovery::entries(root).unwrap_or_default() {
            acc.add(ResourceType::Extensions, &entry, &meta, true);
        }
    }
}

/// The resources of the package installed at `root`, as [`resolve`] finds
/// them for a settings entry with `filter`, each enabled or not. `local`
/// says whether its source is a local path.
pub fn package_resources(
    root: &Path,
    filter: Option<&FilteredPackage>,
    local: bool,
) -> ResolvedPaths {
    let mut acc = ResolvedPaths::default();
    collect_package(
        &mut acc,
        root,
        filter,
        local,
        metadata("local", "user", "package", None),
    );
    acc
}

/// pi's `resolve`: packages, then settings entries, then discovered files,
/// then built-in extensions; ordered by precedence without repeats.
pub fn resolve(input: &ResolveInput<'_>) -> ResolvedPaths {
    let mut acc = ResolvedPaths::default();
    for package in &input.packages {
        collect_package(
            &mut acc,
            &package.root,
            package.filter.as_ref(),
            super::source::is_local(&package.source),
            metadata(&package.source, &package.scope, "package", None),
        );
    }
    for (index, kind) in ResourceType::ALL.into_iter().enumerate() {
        let project_meta = metadata("local", "project", "top-level", None);
        let user_meta = metadata("local", "user", "top-level", None);
        local_entries(
            input.project[index],
            kind,
            &mut acc,
            &project_meta,
            &input.project_dir,
        );
        local_entries(
            input.user[index],
            kind,
            &mut acc,
            &user_meta,
            &input.agent_dir,
        );
    }
    auto_discovered(input, &mut acc);
    let [user_ext, ..] = input.user;
    let [project_ext, ..] = input.project;
    for name in input.builtins {
        let path = PathBuf::from(format!("{BUILTIN_PREFIX}{name}"));
        let overrides: Vec<String> = project_ext
            .iter()
            .filter(|entry| is_override(entry))
            .cloned()
            .collect();
        let project = apply_autoload_disabled_patterns(
            std::slice::from_ref(&path),
            &overrides,
            &input.project_dir,
        )
        .into_iter()
        .next()
        .map(|(_, enabled)| enabled);
        let meta = metadata(
            "builtin",
            if project.is_some() { "project" } else { "user" },
            "top-level",
            None,
        );
        let enabled =
            project.unwrap_or_else(|| is_enabled_by_overrides(&path, user_ext, &input.agent_dir));
        acc.add(ResourceType::Extensions, &path, &meta, enabled);
    }
    for kind in ResourceType::ALL {
        let list = acc.of_mut(kind);
        list.sort_by_key(|resource| precedence(&resource.info));
        let mut seen = HashSet::new();
        list.retain(|resource| {
            let canonical = std::fs::canonicalize(&resource.info.path)
                .unwrap_or_else(|_| PathBuf::from(&resource.info.path));
            seen.insert(canonical)
        });
    }
    acc
}

/// The `extensions`, `skills`, `prompts` and `themes` lists of a settings
/// document.
pub fn settings_lists(document: &serde_json::Map<String, serde_json::Value>) -> [Vec<String>; 4] {
    ResourceType::ALL.map(|kind| {
        document
            .get(kind.key())
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect()
    })
}

/// pi's `dedupePackages` and `findAutoloadDeltaBase`: the `packages` entries
/// of the project, then the user, that are installed, as `install_path`
/// places a source written in a scope's settings. A project entry wins over
/// the user's for the same package, unless it sets `autoload: false`: then
/// it only overrides the resources it names, on top of the user's entry.
pub fn package_inputs(
    project: &[serde_json::Value],
    user: &[serde_json::Value],
    base: impl Fn(&str) -> PathBuf,
    install_path: impl Fn(&super::source::Source, &str) -> PathBuf,
) -> Vec<PackageInput> {
    use super::source::parse;
    let source_of = |entry: &serde_json::Value| super::entry_source(entry).map(str::to_owned);
    let identity = |source: &str, scope: &str| parse(source).identity(&base(scope));
    let delta = |entry: &serde_json::Value| {
        entry.is_object() && entry["autoload"] == serde_json::Value::Bool(false)
    };
    let mut result: Vec<(serde_json::Value, &str)> = Vec::new();
    let mut seen: Vec<(String, usize)> = Vec::new();
    let entries = project
        .iter()
        .map(|entry| (entry, "project"))
        .chain(user.iter().map(|entry| (entry, "user")));
    for (entry, scope) in entries {
        let Some(source) = source_of(entry) else {
            continue;
        };
        let id = identity(&source, scope);
        match seen
            .iter()
            .find(|(seen_id, _)| *seen_id == id)
            .map(|(_, index)| *index)
        {
            None => {
                seen.push((id, result.len()));
                result.push((entry.clone(), scope));
            }
            Some(index) => {
                let (existing, existing_scope) = &result[index];
                if *existing_scope == "project" && scope == "user" {
                    if delta(existing) {
                        result.push((entry.clone(), scope));
                    }
                } else if scope == "project" {
                    result[index] = (entry.clone(), scope);
                }
            }
        }
    }
    let mut inputs = Vec::new();
    for (entry, scope) in &result {
        let Some(source) = source_of(entry) else {
            continue;
        };
        let filter: Option<FilteredPackage> = entry
            .is_object()
            .then(|| serde_json::from_value(entry.clone()).ok())
            .flatten();
        let (located, located_scope) = if *scope == "project" && delta(entry) {
            let id = identity(&source, scope);
            result
                .iter()
                .filter(|(_, other)| *other == "user")
                .filter_map(|(other, _)| source_of(other))
                .find(|other| identity(other, "user") == id)
                .map_or((source.clone(), *scope), |other| (other, "user"))
        } else {
            (source.clone(), *scope)
        };
        let root = install_path(&parse(&located), located_scope);
        if root.exists() {
            inputs.push(PackageInput {
                source,
                scope: (*scope).to_owned(),
                root,
                filter,
            });
        }
    }
    inputs
}
