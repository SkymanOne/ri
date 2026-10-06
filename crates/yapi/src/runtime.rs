//! Session replacement shared by the interactive and RPC modes: the session
//! files that pi's `AgentSessionRuntime` (`core/agent-session-runtime.ts` in pi
//! `v1.0.0`) builds for a new session, a fork or clone, and a switch. Each mode
//! settles the current session first and builds the replacement through its
//! [`SessionFactory`].

use std::path::{Path, PathBuf};

use yapi_core::agent_session::AgentSession;
use yapi_core::session::SessionManager;
use yapi_types::message::Message;
use yapi_types::session::FileEntry;

/// Builds a session around a session file, with the startup options of the
/// process.
pub type SessionFactory = Box<dyn Fn(SessionManager) -> anyhow::Result<AgentSession>>;

/// A session's file as pi's events report it; `None` in memory.
pub fn file_of(manager: &SessionManager) -> Option<String> {
    manager.file().map(|file| file.display().to_string())
}

/// The file for pi's `newSession`: a new one beside the current session's, or
/// in memory when the current one is, linked to `parent` when given.
pub fn new_session(
    current: &AgentSession,
    parent: Option<String>,
) -> anyhow::Result<SessionManager> {
    let (persisted, cwd, dir) = current.with_session(|session| {
        (
            session.is_persisted(),
            session.cwd().to_path_buf(),
            session.dir().to_path_buf(),
        )
    });
    let mut manager = if persisted {
        SessionManager::create(&cwd, &dir, None)?
    } else {
        SessionManager::in_memory(&cwd)
    };
    if parent.is_some() {
        manager.new_session(None, parent);
    }
    Ok(manager)
}

/// A checked fork, built once the current session has settled.
pub struct Fork {
    /// The new leaf; `None` starts an empty session linked to the current one.
    target: Option<String>,
    /// The forked user message's text: `/fork` puts it back in the editor.
    pub text: Option<String>,
}

/// pi's `fork` checks: `at` keeps `entry_id` (`/clone`); otherwise the entry
/// must be a user message, and the fork ends before it.
pub fn plan_fork(current: &AgentSession, entry_id: &str, at: bool) -> Result<Fork, String> {
    current.with_session(|session| {
        let invalid = || "Invalid entry ID for forking".to_owned();
        let entry = session.entry(entry_id).ok_or_else(invalid)?;
        if at {
            return Ok(Fork {
                target: Some(entry_id.to_owned()),
                text: None,
            });
        }
        match entry {
            FileEntry::Message(message) => match &message.message {
                Message::User(user) => Ok(Fork {
                    target: message.meta.parent_id.clone(),
                    text: Some(user.content.text("")),
                }),
                _ => Err(invalid()),
            },
            _ => Err(invalid()),
        }
    })
}

impl Fork {
    /// The forked session file. An in-memory session is forked in place, so
    /// `current` keeps an empty one.
    pub fn build(&self, current: &AgentSession) -> Result<SessionManager, String> {
        let (persisted, file, cwd, dir) = current.with_session(|session| {
            (
                session.is_persisted(),
                session.file().map(Path::to_path_buf),
                session.cwd().to_path_buf(),
                session.dir().to_path_buf(),
            )
        });
        if !persisted {
            let mut manager = current.take_session();
            match &self.target {
                None => {
                    manager.new_session(None, None);
                }
                Some(target) => {
                    manager
                        .create_branched_session(target)
                        .map_err(|error| error.to_string())?;
                }
            }
            return Ok(manager);
        }
        let file = file.ok_or("Persisted session is missing a session file")?;
        match &self.target {
            None => {
                let mut manager =
                    SessionManager::create(&cwd, &dir, None).map_err(|error| error.to_string())?;
                manager.new_session(None, Some(file.display().to_string()));
                Ok(manager)
            }
            Some(target) => {
                if !file.exists() {
                    return Err("This session has not been saved yet. Send a message before cloning or forking it.".into());
                }
                let mut manager = SessionManager::open(&file, Some(&dir), None)
                    .map_err(|error| error.to_string())?;
                manager
                    .create_branched_session(target)
                    .map_err(|error| error.to_string())?
                    .ok_or("Failed to create forked session")?;
                Ok(manager)
            }
        }
    }
}

/// Why a session could not be switched to.
#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    /// The file could not be read.
    #[error("{0}")]
    Open(String),
    /// The session's working directory is gone; pi's `MissingSessionCwdError`.
    #[error(
        "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
        session_cwd.display(),
        file.display(),
        fallback.display()
    )]
    MissingCwd {
        /// The session file.
        file: PathBuf,
        /// The directory it was recorded in.
        session_cwd: PathBuf,
        /// The directory it would run in instead.
        fallback: PathBuf,
    },
}

/// pi's `switchSession`: opens `path`, in `cwd_override` when given, and
/// refuses a session whose directory no longer exists.
pub fn open_session(
    path: &Path,
    cwd_override: Option<&Path>,
    fallback: &Path,
) -> Result<SessionManager, SwitchError> {
    // Reading a directory fails as Node's `readFile` does in pi.
    if path.is_dir() {
        return Err(SwitchError::Open(
            "EISDIR: illegal operation on a directory, read".into(),
        ));
    }
    let manager = SessionManager::open(path, None, cwd_override)
        .map_err(|error| SwitchError::Open(error.to_string()))?;
    if !manager.cwd().as_os_str().is_empty() && !manager.cwd().exists() {
        return Err(SwitchError::MissingCwd {
            file: manager
                .file()
                .map_or_else(|| path.to_path_buf(), Path::to_path_buf),
            session_cwd: manager.cwd().to_path_buf(),
            fallback: fallback.to_path_buf(),
        });
    }
    Ok(manager)
}
