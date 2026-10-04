//! `ri import pi`: copies pi's state into ri's directories. ri reads pi's
//! formats unchanged, so files are copied as they are; files ri already has
//! are kept.

use std::path::{Path, PathBuf};

/// The agent directory entries ri reads, in the order they are copied.
pub const AGENT_ENTRIES: &[&str] = &[
    "settings.json",
    "auth.json",
    "models.json",
    "keybindings.json",
    "mcp.json",
    "trust.json",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
    "sessions",
    "prompts",
    "skills",
    "themes",
    "extensions",
    "npm",
    "git",
];

/// The project directory (`.pi`) entries ri reads, in the order they are
/// copied.
pub const PROJECT_ENTRIES: &[&str] = &[
    "settings.json",
    "mcp.json",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "prompts",
    "skills",
    "themes",
    "extensions",
    "npm",
    "git",
];

/// What happened to one entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    /// The entry's name.
    pub entry: String,
    /// Files copied.
    pub copied: usize,
    /// Files ri already had, left as they are.
    pub kept: usize,
}

/// pi's agent directory: `PI_CODING_AGENT_DIR`, or `~/.pi/agent`.
pub fn pi_agent_dir() -> PathBuf {
    match std::env::var("PI_CODING_AGENT_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(crate::tools::path::expand(&dir)),
        _ => crate::tools::path::home_dir().join(".pi").join("agent"),
    }
}

/// An I/O error naming the path it concerns.
fn at(path: &Path, error: std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Copies `from` to `to`, following symbolic links. Links that lead nowhere,
/// loop, or lead back into a directory being copied are skipped.
fn copy_tree(
    from: &Path,
    to: &Path,
    imported: &mut Imported,
    ancestors: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    let metadata = match std::fs::metadata(from) {
        Ok(metadata) => metadata,
        Err(_) if from.symlink_metadata().is_ok_and(|link| link.is_symlink()) => return Ok(()),
        Err(error) => return Err(at(from, error)),
    };
    if metadata.is_dir() {
        let real = std::fs::canonicalize(from).map_err(|error| at(from, error))?;
        if ancestors.contains(&real) {
            return Ok(());
        }
        std::fs::create_dir_all(to).map_err(|error| at(to, error))?;
        let mut entries: Vec<_> = std::fs::read_dir(from)
            .map_err(|error| at(from, error))?
            .flatten()
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        ancestors.push(real);
        for entry in entries {
            copy_tree(
                &entry.path(),
                &to.join(entry.file_name()),
                imported,
                ancestors,
            )?;
        }
        ancestors.pop();
        return Ok(());
    }
    if to.exists() {
        imported.kept += 1;
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| at(parent, error))?;
    }
    // `copy` keeps the permissions, so credentials stay private.
    std::fs::copy(from, to).map_err(|error| at(from, error))?;
    imported.copied += 1;
    Ok(())
}

/// Copies `entries` of directory `from` into `to`, keeping files `to`
/// already has. Entries `from` lacks are skipped.
pub fn import(from: &Path, to: &Path, entries: &[&str]) -> std::io::Result<Vec<Imported>> {
    let mut out = Vec::new();
    for entry in entries {
        let source = from.join(entry);
        if source.symlink_metadata().is_err() {
            continue;
        }
        let mut imported = Imported {
            entry: (*entry).to_owned(),
            ..Imported::default()
        };
        copy_tree(&source, &to.join(entry), &mut imported, &mut Vec::new())?;
        out.push(imported);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn copies_what_ri_reads_and_keeps_existing_files() {
        let dir = std::env::temp_dir().join(format!("ri-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (pi, ri) = (dir.join("pi"), dir.join("ri"));
        write(&pi, "settings.json", "{\"theme\":\"light\"}\n");
        write(&pi, "auth.json", "{}\n");
        write(&pi, "sessions/--a--/one.jsonl", "{}\n");
        write(&pi, "sessions/--a--/two.jsonl", "{}\n");
        write(&pi, "bin/fd", "binary");
        write(&pi, "pi-debug.log", "log");
        write(&ri, "auth.json", "{\"mine\":true}\n");

        let imported = import(&pi, &ri, AGENT_ENTRIES).unwrap();

        let summary: Vec<(&str, usize, usize)> = imported
            .iter()
            .map(|entry| (entry.entry.as_str(), entry.copied, entry.kept))
            .collect();
        assert_eq!(
            summary,
            [
                ("settings.json", 1, 0),
                ("auth.json", 0, 1),
                ("sessions", 2, 0)
            ]
        );
        assert_eq!(
            std::fs::read_to_string(ri.join("auth.json")).unwrap(),
            "{\"mine\":true}\n"
        );
        assert!(ri.join("sessions/--a--/two.jsonl").is_file());
        assert!(!ri.join("bin").exists());
        assert!(!ri.join("pi-debug.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn copies_context_files_and_survives_bad_links() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("ri-import-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (pi, ri) = (dir.join("pi"), dir.join("ri"));
        write(&pi, "AGENTS.md", "global rules\n");
        write(&pi, "skills/real/SKILL.md", "skill\n");
        symlink(pi.join("nowhere"), pi.join("skills/dangling")).unwrap();
        symlink(pi.join("skills/loop-b"), pi.join("skills/loop-a")).unwrap();
        symlink(pi.join("skills/loop-a"), pi.join("skills/loop-b")).unwrap();
        symlink(pi.join("skills"), pi.join("skills/real/up")).unwrap();
        symlink(pi.join("skills/real"), pi.join("skills/alias")).unwrap();

        let imported = import(&pi, &ri, AGENT_ENTRIES).unwrap();

        // On a case-insensitive file system `AGENTS.MD` names the same file
        // and is kept.
        let summary: Vec<(&str, usize)> = imported
            .iter()
            .filter(|entry| entry.copied > 0)
            .map(|entry| (entry.entry.as_str(), entry.copied))
            .collect();
        assert_eq!(summary, [("AGENTS.md", 1), ("skills", 2)]);
        assert!(ri.join("skills/alias/SKILL.md").is_file());
        assert!(!ri.join("skills/dangling").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
