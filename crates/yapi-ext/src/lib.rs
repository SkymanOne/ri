#![doc = env!("CARGO_PKG_DESCRIPTION")]
//!
//! Extensions run in instances of `yapi-js`, a WebAssembly component holding a
//! QuickJS-NG runtime with Node shims and pi's extension API. The host here
//! compiles the component once per [`Engine`], runs each [`Instance`] on its own
//! thread under a memory limit and a compute limit, resolves and transpiles
//! modules, and answers the guest's requests within its [`Grants`].

pub mod codemode;
mod engine;
mod extensions;
mod instance;
mod loader;
mod ops;
mod requests;
mod streams;

pub use engine::Engine;
pub use extensions::{ExtensionHost, Flag, LoadError, RegisteredProvider};
pub use instance::{Bridge, Grants, Instance, NoBridge, Options, join_stopped};

/// Why the extension runtime failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The runtime component could not be compiled or linked.
    #[error("cannot compile the extension runtime: {0}")]
    Compile(String),
    /// An instance could not start.
    #[error("cannot start the extension runtime: {0}")]
    Instantiate(String),
    /// The call failed in JS; the message is the error's.
    #[error("{0}")]
    Call(String),
    /// The instance trapped during the call and was restarted.
    #[error("extension runtime stopped: {0}")]
    Crashed(String),
    /// The instance is gone.
    #[error("the extension runtime has stopped")]
    Stopped,
}

impl Error {
    fn compile(err: impl std::fmt::Display) -> Error {
        Error::Compile(format!("{err:#}"))
    }
}
