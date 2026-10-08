//! Resolving paths the model writes: `@` prefixes, `~`, `file://`, odd spaces, and
//! the macOS screenshot and quote variants of existing files.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// The user's home directory.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Normalizes a path as pi does for tool input: Unicode spaces become spaces, a
/// leading `@` is dropped, `~` expands, and `file://` URLs become paths.
pub fn normalize(input: &str) -> String {
    let mut text: String = input
        .chars()
        .map(|c| if is_unicode_space(c) { ' ' } else { c })
        .collect();
    if let Some(rest) = text.strip_prefix('@') {
        text = rest.to_owned();
    }
    expand(&text)
}

/// Expands `~` and `file://` URLs.
pub fn expand(text: &str) -> String {
    // pi's `fileURLToPath`.
    if text.starts_with("file://") {
        return url::Url::parse(text)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .map_or_else(
                || text.to_owned(),
                |path| path.to_string_lossy().into_owned(),
            );
    }
    expand_home(text)
}

/// Node's `pathToFileURL` for an absolute path.
pub fn file_url(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(String::from)
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// `~` and `~/…` name the home directory, as in a shell.
pub fn expand_home(text: &str) -> String {
    if text == "~" {
        return home_dir().to_string_lossy().into_owned();
    }
    match text.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest).to_string_lossy().into_owned(),
        None => text.to_owned(),
    }
}

/// Lexically resolves `path` against `base`: absolute result, no `.` or `..`.
pub fn resolve_lexically(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Node's `path.relative` for absolute, lexically resolved paths: the steps from
/// `from` to `to`, `""` when they are the same.
pub fn relative(from: &Path, to: &Path) -> String {
    use std::path::Component;
    let parts = |path: &Path| -> Vec<String> {
        resolve_lexically(Path::new("/"), path)
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    let (from, to) = (parts(from), parts(to));
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut steps: Vec<String> = vec!["..".to_owned(); from.len() - common];
    steps.extend(to[common..].iter().cloned());
    steps.join("/")
}

/// Resolves a tool path against the working directory.
pub fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
    resolve_lexically(cwd, Path::new(&normalize(path)))
}

/// Like [`resolve_to_cwd`], but when the file does not exist, tries the variants
/// pi tries: narrow no-break space before AM/PM, NFD, curly apostrophes.
pub fn resolve_read_path(path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(path, cwd);
    if resolved.exists() {
        return resolved;
    }
    let text = resolved.to_string_lossy().into_owned();
    let am_pm = text
        .replace(" AM.", "\u{202F}AM.")
        .replace(" PM.", "\u{202F}PM.")
        .replace(" am.", "\u{202F}am.")
        .replace(" pm.", "\u{202F}pm.");
    let nfd: String = text.nfd().collect();
    let curly = text.replace('\'', "\u{2019}");
    let nfd_curly = nfd.replace('\'', "\u{2019}");
    for candidate in [am_pm, nfd, curly, nfd_curly] {
        if candidate != text && Path::new(&candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_like_pi() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            resolve_to_cwd("@src/../lib.rs", cwd),
            PathBuf::from("/work/project/lib.rs")
        );
        assert_eq!(resolve_to_cwd("/tmp/./a", cwd), PathBuf::from("/tmp/a"));
        assert_eq!(
            resolve_to_cwd("file:///tmp/a%20b", cwd),
            PathBuf::from("/tmp/a b")
        );
        assert_eq!(normalize("a\u{00A0}b"), "a b");
    }

    #[test]
    fn relative_paths_like_node() {
        assert_eq!(
            relative(Path::new("/a/b"), Path::new("/a/b/c/d.txt")),
            "c/d.txt"
        );
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative(Path::new("/a/b/"), Path::new("/a/b")), "");
    }
}
