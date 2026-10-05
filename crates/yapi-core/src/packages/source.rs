//! Package sources: `npm:` specs, git repositories and local paths, as pi's
//! `parseSource` reads them (`core/package-manager.ts`, `utils/git.ts`).

use std::path::{Path, PathBuf};

use crate::tools::path::{expand, resolve_to_cwd};

/// Where a package comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// An npm package: `npm:<name>[@<version or range>]`.
    Npm {
        /// The package name.
        name: String,
        /// The requested version or range.
        version: Option<String>,
        /// Whether the version is exact, so updates skip it.
        pinned: bool,
    },
    /// A git repository.
    Git {
        /// What `git clone` gets.
        repo: String,
        /// Host name, such as `github.com`.
        host: String,
        /// Repository path without `.git`, such as `user/repo`.
        path: String,
        /// The branch, tag or commit to check out.
        reference: Option<String>,
    },
    /// A file or directory.
    Local {
        /// The path as written.
        path: String,
    },
}

/// Prefixes that make a source something other than a local path.
const REMOTE_PREFIXES: [&str; 7] = [
    "npm:", "git:", "github:", "http:", "https:", "ssh:", "builtin:",
];

/// Parses `source` as pi does: `npm:`, then git forms, then a local path.
pub fn parse(source: &str) -> Source {
    if let Some(spec) = source.strip_prefix("npm:") {
        return npm(spec.trim());
    }
    if let Some(git) = git(source) {
        return git;
    }
    Source::Local {
        path: source.to_owned(),
    }
}

fn npm(spec: &str) -> Source {
    let (name, version) = split_npm_spec(spec);
    let pinned = version
        .as_deref()
        .is_some_and(|version| semver::Version::parse(version).is_ok());
    Source::Npm {
        name,
        version,
        pinned,
    }
}

/// Splits `<name>[@<version or range>]` into the name and the version.
pub(crate) fn split_npm_spec(spec: &str) -> (String, Option<String>) {
    // `^(@?[^@]+(?:/[^@]+)?)(?:@(.+))?$`: a scoped name keeps its leading `@`.
    let (scope, rest) = match spec.strip_prefix('@') {
        Some(rest) => ("@", rest),
        None => ("", spec),
    };
    let (name, version) = match rest.split_once('@') {
        Some((name, version)) if !version.is_empty() => (name, Some(version.to_owned())),
        _ => (rest, None),
    };
    (format!("{scope}{name}"), version)
}

/// Splits `@ref` (or `#ref`) off the end of a repository path.
fn split_reference(path: &str) -> (&str, Option<String>) {
    if let Some((path, reference)) = path.rsplit_once('#') {
        return (path, Some(reference.to_owned()));
    }
    let last = path.rfind('/').map_or(0, |index| index + 1);
    match path[last..].find('@') {
        Some(at) => (&path[..last + at], Some(path[last + at + 1..].to_owned())),
        None => (path, None),
    }
}

fn clean_path(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    let valid = segments.len() >= 2
        && segments
            .iter()
            .all(|segment| !segment.is_empty() && *segment != "..")
        && !path.contains('\\')
        && !path.contains('\0');
    valid.then(|| path.to_owned())
}

fn git(source: &str) -> Option<Source> {
    let (explicit, rest) = match source.strip_prefix("git:") {
        Some(rest) => (true, rest.trim()),
        None => (false, source.trim()),
    };
    let has_scheme = ["https://", "http://", "ssh://", "git://"]
        .iter()
        .any(|scheme| rest.starts_with(scheme));
    if !explicit && !has_scheme {
        return None;
    }
    if has_scheme {
        let parsed = url::Url::parse(rest).ok()?;
        let host = parsed.host_str()?.to_owned();
        let (path, reference) = split_reference(parsed.path());
        let path = clean_path(path)?;
        let mut repo = parsed.clone();
        repo.set_path(&format!("/{path}"));
        repo.set_fragment(None);
        let repo = repo.to_string();
        return Some(Source::Git {
            repo: repo.strip_suffix('/').unwrap_or(&repo).to_owned(),
            host,
            path,
            reference,
        });
    }
    // `git@host:user/repo` or `host/user/repo`.
    let (repo_prefix, host, path) = if let Some((user_host, path)) = rest.split_once(':') {
        let host = user_host.rsplit('@').next()?.to_owned();
        (Some(user_host.to_owned()), host, path)
    } else {
        let (host, path) = rest.split_once('/')?;
        (None, host.to_owned(), path)
    };
    if !(host.contains('.') || host == "localhost") {
        return None;
    }
    let (path, reference) = split_reference(path);
    let path = clean_path(path)?;
    let repo = match repo_prefix {
        Some(user_host) => format!("{user_host}:{path}"),
        None => format!("https://{host}/{path}"),
    };
    Some(Source::Git {
        repo,
        host,
        path,
        reference,
    })
}

/// Whether `source` names a local path.
pub fn is_local(source: &str) -> bool {
    !REMOTE_PREFIXES
        .iter()
        .any(|prefix| source.starts_with(prefix))
        && matches!(parse(source), Source::Local { .. })
}

/// A local source as an absolute path: `~` expands, `file://` URLs convert,
/// and relative paths resolve against `base`.
pub fn local_path(path: &str, base: &Path) -> PathBuf {
    let path = path.strip_prefix("file://").unwrap_or(path);
    resolve_to_cwd(&expand(path), base)
}

impl Source {
    /// The identity settings entries are matched by: the npm name, the git
    /// host and path, or the absolute local path resolved against `base`.
    pub fn identity(&self, base: &Path) -> String {
        match self {
            Source::Npm { name, .. } => format!("npm:{name}"),
            Source::Git { host, path, .. } => format!("git:{host}/{path}"),
            Source::Local { path } => format!("local:{}", local_path(path, base).display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_npm_specs() {
        assert_eq!(
            parse("npm:@scope/pkg@1.2.3"),
            Source::Npm {
                name: "@scope/pkg".into(),
                version: Some("1.2.3".into()),
                pinned: true
            }
        );
        assert_eq!(
            parse("npm:pkg@^1"),
            Source::Npm {
                name: "pkg".into(),
                version: Some("^1".into()),
                pinned: false
            }
        );
        assert_eq!(
            parse("npm: pkg "),
            Source::Npm {
                name: "pkg".into(),
                version: None,
                pinned: false
            }
        );
    }

    #[test]
    fn parses_git_forms() {
        let expected = |repo: &str, reference: Option<&str>| Source::Git {
            repo: repo.into(),
            host: "github.com".into(),
            path: "user/repo".into(),
            reference: reference.map(str::to_owned),
        };
        assert_eq!(
            parse("git:github.com/user/repo@v1"),
            expected("https://github.com/user/repo", Some("v1"))
        );
        assert_eq!(
            parse("git:git@github.com:user/repo.git"),
            expected("git@github.com:user/repo", None)
        );
        assert_eq!(
            parse("https://github.com/user/repo.git"),
            expected("https://github.com/user/repo", None)
        );
        assert_eq!(
            parse("ssh://git@github.com/user/repo@main"),
            expected("ssh://git@github.com/user/repo", Some("main"))
        );
        // Without `git:`, only URLs are git sources.
        assert!(matches!(
            parse("github.com/user/repo"),
            Source::Local { .. }
        ));
        assert!(matches!(parse("git:github.com/repo"), Source::Local { .. }));
        assert!(matches!(parse("github:user/repo"), Source::Local { .. }));
    }

    #[test]
    fn identities_ignore_versions_and_forms() {
        let base = Path::new("/base");
        assert_eq!(
            parse("npm:pkg@1.0.0").identity(base),
            parse("npm:pkg").identity(base)
        );
        assert_eq!(
            parse("git:git@github.com:user/repo").identity(base),
            parse("https://github.com/user/repo@v2").identity(base)
        );
        assert_eq!(parse("./pkg").identity(base), "local:/base/pkg");
    }
}
