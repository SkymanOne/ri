//! `yapi import pi`: copies pi's agent directory, and the current project's
//! `.pi` directory, into yapi's.

use std::io::Write as _;
use std::path::Path;

use yapi_core::import::{AGENT_ENTRIES, Imported, PROJECT_ENTRIES, import, pi_agent_dir};

use crate::{err, out};

const USAGE: &str = "Usage:\n  yapi import pi\n\nCopy pi's settings, credentials, models, keybindings, MCP servers, sessions,\nprompts, skills, themes, extensions and packages into yapi. Files yapi already has\nare kept. The current project's .pi directory is copied into .yapi.\n";

/// "file" or "files" for `count`.
fn files(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

/// Prints what one directory's import did; the files copied and kept.
fn report(from: &Path, to: &Path, imported: &[Imported]) -> (usize, usize) {
    out(&format!("{} -> {}", from.display(), to.display()));
    if imported.is_empty() {
        out("  nothing to import");
    }
    for entry in imported {
        let mut line = format!(
            "  {}: {} {} copied",
            entry.entry,
            entry.copied,
            files(entry.copied)
        );
        if entry.kept > 0 {
            line.push_str(&format!(
                ", {} existing {} kept",
                entry.kept,
                files(entry.kept)
            ));
        }
        out(&line);
    }
    imported.iter().fold((0, 0), |(copied, kept), entry| {
        (copied + entry.copied, kept + entry.kept)
    })
}

/// Runs `yapi import` with `args` (after `import`); the exit code.
pub fn run(args: &[String]) -> u8 {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            let _ = write!(std::io::stdout(), "{USAGE}");
            return 0;
        }
        Some("pi") if args.len() == 1 => {}
        _ => {
            err("Usage: yapi import pi");
            return 1;
        }
    }
    let from = pi_agent_dir();
    if !from.is_dir() {
        err(&format!("No pi state found at {}", from.display()));
        return 1;
    }
    let to = yapi_core::config::agent_dir();
    let mut totals = (0, 0);
    let mut copy = |from: &Path, to: &Path, entries: &[&str]| match import(from, to, entries) {
        Ok(imported) => {
            let (copied, kept) = report(from, to, &imported);
            totals = (totals.0 + copied, totals.1 + kept);
            true
        }
        Err(error) => {
            err(&format!("Error: {error}"));
            false
        }
    };
    if !copy(&from, &to, AGENT_ENTRIES) {
        return 1;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let project = cwd.join(".pi");
        if project.is_dir()
            && project != from
            && !copy(&project, &cwd.join(".yapi"), PROJECT_ENTRIES)
        {
            return 1;
        }
    }
    out(&format!(
        "Imported {} {}; kept {} existing {}.",
        totals.0,
        files(totals.0),
        totals.1,
        files(totals.1)
    ));
    0
}
