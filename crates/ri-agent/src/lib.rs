#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]

pub mod hooks;
pub mod run;
pub mod tool;

pub use hooks::AgentHooks;
pub use run::{AgentContext, LoopConfig, StreamFn, run, run_continue};
pub use tool::{ExecutionMode, Tool, UpdateSink};
