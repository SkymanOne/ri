//! Helpers shared by the binary's tests.
#![allow(
    clippy::unwrap_used,
    dead_code,
    reason = "test helpers; each test file uses some"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository root.
pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// `tests/fixtures`.
pub fn fixtures() -> PathBuf {
    repo().join("tests/fixtures")
}

/// A new empty directory for one test.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The binary in `home`, with a cleared environment whose home is `home` and
/// whose agent directory is `home/agent`.
pub fn yapi(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yapi"));
    command
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("YAPI_CODING_AGENT_DIR", home.join("agent"));
    command
}
