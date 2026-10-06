//! Session replacement shared by the modes: the session files that pi's
//! `AgentSessionRuntime` (`core/agent-session-runtime.ts` in pi `v1.0.0`)
//! builds for a new session, a fork or clone, and a switch, and the runtime
//! of the RPC and print modes. Each mode settles the current session first
//! and builds the replacement through its [`SessionFactory`].

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures_util::future::LocalBoxFuture;
use tokio::sync::{mpsc, oneshot};
use yapi_core::agent_session::{AgentSession, Replacement, SessionChange};
use yapi_core::extensions::{SessionAction, SessionActions};
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

/// An extension command's session change, with where its outcome goes.
pub type ActionRequest = (SessionAction, oneshot::Sender<Result<bool, String>>);

/// [`SessionActions`] that `send` each change to a mode's loop and wait for
/// its outcome there; `send` says whether the loop took it.
pub fn actions(send: impl Fn(ActionRequest) -> bool + Send + Sync + 'static) -> SessionActions {
    Arc::new(move |action| {
        let (reply, outcome) = oneshot::channel();
        let sent = send((action, reply));
        Box::pin(async move {
            if !sent {
                return Err("The session has ended".to_owned());
            }
            outcome
                .await
                .unwrap_or_else(|_| Err("The session has ended".to_owned()))
        })
    })
}

/// The session another replaced and why, as `session_start` reports it.
pub type Replaced = Option<(Replacement, Option<String>)>;

/// Makes a session current in a mode: streams its events and starts its
/// extensions, telling them the session it replaced.
pub type Bind = Box<dyn Fn(AgentSession, Replaced) -> LocalBoxFuture<'static, ()>>;

/// pi's `AgentSessionRuntime` for the RPC and print modes: the current
/// session, which session changes replace through the factory. Runs on a
/// `LocalSet`.
pub struct Runtime {
    /// The current session and the session it replaced.
    current: RefCell<(AgentSession, Replaced)>,
    factory: SessionFactory,
    bind: Bind,
    actions: SessionActions,
}

impl Runtime {
    /// A runtime whose extension commands change sessions through it, which
    /// starts with `session` current.
    pub async fn start(session: AgentSession, factory: SessionFactory, bind: Bind) -> Rc<Runtime> {
        let (sender, mut receiver) = mpsc::unbounded_channel::<ActionRequest>();
        let runtime = Rc::new(Runtime {
            current: RefCell::new((session.clone(), None)),
            factory,
            bind,
            actions: actions(move |request| sender.send(request).is_ok()),
        });
        let weak = Rc::downgrade(&runtime);
        tokio::task::spawn_local(async move {
            while let Some((action, reply)) = receiver.recv().await {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                // Each change runs beside the command that asked for it.
                tokio::task::spawn_local(async move {
                    let _ = reply.send(runtime.act(action).await);
                });
            }
        });
        runtime.bind(session, None).await;
        runtime
    }

    /// The current session.
    pub fn session(&self) -> AgentSession {
        self.current.borrow().0.clone()
    }

    /// Makes `session`, which replaced another as `replaced` says, current
    /// and starts its extensions.
    async fn bind(&self, session: AgentSession, replaced: Replaced) {
        *self.current.borrow_mut() = (session.clone(), replaced.clone());
        session.set_actions(Arc::clone(&self.actions));
        (self.bind)(session, replaced).await;
    }

    /// Starts the current session's extensions again, as pi's RPC session
    /// commands do after the runtime did.
    pub async fn rebind(&self) {
        let (session, replaced) = self.current.borrow().clone();
        self.bind(session, replaced).await;
    }

    /// Carries out an extension command's session change, as pi's RPC and
    /// print modes do: whether an extension cancelled it.
    async fn act(&self, action: SessionAction) -> Result<bool, String> {
        match action {
            SessionAction::New { parent } => self.new_session(parent).await,
            SessionAction::Fork { entry_id, at } => Ok(self.fork(&entry_id, at).await?.is_none()),
            SessionAction::Tree { target_id, options } => self
                .session()
                .navigate_tree(&target_id, options)
                .await
                .map(|outcome| outcome.cancelled),
            SessionAction::Switch { path } => self.switch_session(&path).await,
            SessionAction::Reload => self.reload().await.map(|()| false),
            SessionAction::Replaced => Ok(false),
        }
    }

    /// pi's runtime replacement, unless an extension cancels `change`, which
    /// this answers.
    async fn change(
        &self,
        change: SessionChange,
        build: impl FnOnce(&AgentSession) -> Result<SessionManager, String>,
    ) -> Result<bool, String> {
        if self.session().cancels(&change).await {
            return Ok(true);
        }
        self.replace(change.reason(), build).await?;
        Ok(false)
    }

    /// pi's runtime replacement: `build` makes the session file, the current
    /// run settles and is persisted, the current session's extensions stop,
    /// and a session built around the file takes over for `reason`.
    async fn replace(
        &self,
        reason: Replacement,
        build: impl FnOnce(&AgentSession) -> Result<SessionManager, String>,
    ) -> Result<(), String> {
        let current = self.session();
        let previous = current.with_session(|manager| file_of(manager));
        let manager = build(&current)?;
        current.abort();
        current.abort_bash();
        current.wait_for_idle().await;
        current.shutdown_for(reason, file_of(&manager)).await;
        let session = (self.factory)(manager).map_err(|error| error.to_string())?;
        if reason == Replacement::Reload {
            session.keep_selection(&current);
        }
        self.bind(session, Some((reason, previous))).await;
        Ok(())
    }

    /// pi's `newSession`: whether an extension cancelled it.
    pub async fn new_session(&self, parent: Option<String>) -> Result<bool, String> {
        self.change(SessionChange::New, |current| {
            new_session(current, parent).map_err(|error| error.to_string())
        })
        .await
    }

    /// pi's `fork` at `entry_id`, keeping the entry when `at`: the forked
    /// user message's text, or `None` when an extension cancelled it.
    pub async fn fork(&self, entry_id: &str, at: bool) -> Result<Option<Option<String>>, String> {
        let mut text = None;
        let change = SessionChange::Fork {
            entry_id: entry_id.to_owned(),
            at,
        };
        let cancelled = self
            .change(change, |current| {
                let fork = plan_fork(current, entry_id, at)?;
                text = fork.text.clone();
                fork.build(current)
            })
            .await?;
        Ok((!cancelled).then_some(text))
    }

    /// pi's `switchSession` to `path`, relative to the working directory:
    /// whether an extension cancelled it.
    pub async fn switch_session(&self, path: &str) -> Result<bool, String> {
        let fallback = self.session().cwd().to_path_buf();
        let resolved = std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| PathBuf::from(path));
        self.change(SessionChange::Resume(path.to_owned()), |_| {
            open_session(&resolved, None, &fallback).map_err(|error| error.to_string())
        })
        .await
    }

    /// pi's `reload`: the session again, with its extensions, resources and
    /// settings loaded anew, keeping its model and thinking level.
    pub async fn reload(&self) -> Result<(), String> {
        self.replace(Replacement::Reload, |current| Ok(current.take_session()))
            .await
    }
}
