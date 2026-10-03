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

fn copy_tree(from: &Path, to: &Path, imported: &mut Imported) -> std::io::Result<()> {
    let metadata = std::fs::metadata(from)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(to)?;
        let mut entries: Vec<_> = std::fs::read_dir(from)?.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            copy_tree(&entry.path(), &to.join(entry.file_name()), imported)?;
        }
        return Ok(());
    }
    if to.exists() {
        imported.kept += 1;
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `copy` keeps the permissions, so credentials stay private.
    std::fs::copy(from, to)?;
    imported.copied += 1;
    Ok(())
}

/// Copies `entries` of directory `from` into `to`, keeping files `to`
/// already has. Entries `from` lacks are skipped.
pub fn import(from: &Path, to: &Path, entries: &[&str]) -> std::io::Result<Vec<Imported>> {
    let mut out = Vec::new();
    for entry in entries {
        let source = from.join(entry);
        if !source.exists() {
            continue;
        }
        let mut imported = Imported {
            entry: (*entry).to_owned(),
            ..Imported::default()
        };
        copy_tree(&source, &to.join(entry), &mut imported)?;
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
}
