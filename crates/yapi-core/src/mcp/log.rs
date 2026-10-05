//! Log messages servers send with `notifications/message`, appended to
//! `mcp.log` in the agent directory and rotated to `mcp.log.1` past 5 MB.
//! Port of `extensions/mcp/log.ts` in pi `v1.0.0`.

use std::io::Write as _;
use std::path::PathBuf;

use serde_json::Value;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// One log line: time, server, level, logger and data; continuation lines
/// are indented.
pub fn format_message(server: &str, params: &Value, time: &str) -> String {
    let wrapped;
    let message = if params.is_object() {
        params
    } else {
        wrapped = serde_json::json!({ "data": params });
        &wrapped
    };
    let level = message["level"].as_str().unwrap_or("info");
    let logger = message["logger"]
        .as_str()
        .filter(|logger| !logger.is_empty())
        .map(|logger| format!(" {logger}:"))
        .unwrap_or_default();
    let data = match &message["data"] {
        Value::String(text) => text.clone(),
        Value::Null if message.get("data").is_none() => "undefined".into(),
        other => yapi_types::json::to_string(other).unwrap_or_default(),
    };
    let text = data.replace("\r\n", "\n").replace('\n', "\n    ");
    format!("{time} [{server}] {level}{logger} {text}\n")
}

/// Appends server log messages to one file; failures are ignored.
#[derive(Clone)]
pub struct Log {
    path: PathBuf,
}

impl Log {
    /// A log at `path`.
    pub fn new(path: PathBuf) -> Log {
        Log { path }
    }

    /// Appends one message from `server`.
    pub fn write(&self, server: &str, params: &Value) {
        let line = format_message(server, params, &crate::time::now_iso());
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let size = std::fs::metadata(&self.path).map_or(0, |meta| meta.len());
        if size > MAX_LOG_BYTES {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_like_pi() {
        assert_eq!(
            format_message(
                "docs",
                &json!({"level": "error", "logger": "db", "data": "a\nb"}),
                "T"
            ),
            "T [docs] error db: a\n    b\n"
        );
        assert_eq!(format_message("x", &json!(5), "T"), "T [x] info 5\n");
    }
}
