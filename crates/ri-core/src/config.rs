//! Where ri keeps its state. Same layout as pi, under ri's own directories.

use std::path::{Path, PathBuf};

use crate::tools::path::{expand, home_dir};

/// Application name in messages and paths.
pub const APP_NAME: &str = "ri";
/// Project configuration directory name.
pub const PROJECT_DIR: &str = ".ri";
/// Environment variable overriding the agent directory.
pub const AGENT_DIR_ENV: &str = "RI_CODING_AGENT_DIR";
/// Environment variable overriding the session directory; `--session-dir` wins.
pub const SESSION_DIR_ENV: &str = "RI_CODING_AGENT_SESSION_DIR";

/// Variables every child process gets, as pi sets them on its own process:
/// the agent markers `PI_CODING_AGENT=true` and `AI_AGENT=ri`, and the proxy
/// from the `httpProxy` setting where the environment has none.
pub fn child_env() -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("PI_CODING_AGENT", "true".to_owned()),
        ("AI_AGENT", APP_NAME.to_owned()),
    ];
    env.extend(ri_ai::http::proxy_env());
    env
}

/// The agent directory: `RI_CODING_AGENT_DIR`, else `~/.ri/agent`.
pub fn agent_dir() -> PathBuf {
    match std::env::var(AGENT_DIR_ENV) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(expand(&dir)),
        _ => home_dir().join(PROJECT_DIR).join("agent"),
    }
}

/// Where sessions for `cwd` live by default:
/// `<agent>/sessions/--<cwd with separators as dashes>--`.
pub fn default_session_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    let text = cwd.to_string_lossy();
    let trimmed = text.trim_start_matches(['/', '\\']);
    let safe: String = trimmed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    agent_dir.join("sessions").join(format!("--{safe}--"))
}

/// Executables the agent installs for its tools, prepended to `PATH` for commands.
pub fn bin_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_dirs_match_pi() {
        assert_eq!(
            default_session_dir(Path::new("/h/.ri/agent"), Path::new("/home/user/ri")),
            PathBuf::from("/h/.ri/agent/sessions/--home-user-ri--")
        );
    }
}
