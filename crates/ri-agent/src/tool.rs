//! Tools the agent can call.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use ri_types::event::ToolResult;
use ri_types::message::ToolDeclaration;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// Whether a tool may run alongside other calls from the same response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Runs concurrently with the other calls of its batch.
    #[default]
    Parallel,
    /// Forces the whole batch to run one call at a time.
    Sequential,
}

/// Receives partial results while a tool runs.
pub type UpdateSink = Arc<dyn Fn(ToolResult) + Send + Sync>;

/// A tool: a declaration for the model and an implementation.
///
/// `execute` returns `Err` with a message for failures; the loop turns it into an
/// error result. Cancellation is cooperative through `cancel`.
pub trait Tool: Send + Sync {
    /// Name, description and argument schema as sent to the model.
    fn declaration(&self) -> &ToolDeclaration;

    /// Human-readable name for the UI.
    fn label(&self) -> &str {
        &self.declaration().name
    }

    /// See [`ExecutionMode`].
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// Rewrites raw arguments before validation, for compatibility with older
    /// argument shapes.
    fn prepare_arguments(&self, arguments: Map<String, Value>) -> Map<String, Value> {
        arguments
    }

    /// Runs the tool with validated arguments.
    fn execute(
        &self,
        call_id: String,
        args: Value,
        cancel: CancellationToken,
        updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>>;
}
