#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]

pub mod hooks;
pub mod run;
pub mod tool;

pub use hooks::AgentHooks;
pub use run::{
    AgentContext, LoopConfig, StreamFn, ToolCallOutcome, ToolCallScope, run, run_continue,
    run_tool_call,
};
pub use tool::{ExecutionMode, Tool, UpdateSink};
