//! `write`: create or overwrite a file, creating parent directories.

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_types::event::ToolResult;
use yapi_types::message::ToolDeclaration;

use super::mutation::with_file_lock;
use super::{ToolEnv, check_abort, declaration, node_error, text_result};

/// The `write` tool.
pub struct Write {
    env: ToolEnv,
    declaration: ToolDeclaration,
}

impl Write {
    /// A `write` tool for `env`.
    pub fn new(env: ToolEnv) -> Write {
        Write {
            env,
            declaration: declaration(
                "write",
                "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
                json!({"type":"object","required":["path","content"],"properties":{
                    "path":{"type":"string","description":"Path to the file to write (relative or absolute)"},
                    "content":{"type":"string","description":"Content to write to the file"}}}),
            ),
        }
    }
}

impl Tool for Write {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        cancel: CancellationToken,
        _updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let path = args["path"].as_str().unwrap_or_default().to_owned();
            let content = args["content"].as_str().unwrap_or_default().to_owned();
            let absolute = super::path::resolve_to_cwd(&path, &self.env.cwd);
            with_file_lock(&absolute, async {
                check_abort(&cancel)?;
                if let Some(dir) = absolute.parent() {
                    tokio::fs::create_dir_all(dir)
                        .await
                        .map_err(|err| node_error(&err, "mkdir", dir))?;
                }
                check_abort(&cancel)?;
                tokio::fs::write(&absolute, content)
                    .await
                    .map_err(|err| node_error(&err, "open", &absolute))?;
                check_abort(&cancel)?;
                Ok(text_result(format!("Successfully wrote to {path}"), None))
            })
            .await
        })
    }
}
