//! Blocks `write` and `edit` calls on protected paths: `.env`, `.git/` and
//! `node_modules/`.
//!
//! A port of pi's `protected-paths.ts` example, with the same messages.

use ri_extension_api::{Api, json, notify};

const PROTECTED: [&str; 3] = [".env", ".git/", "node_modules/"];

fn init(api: &mut Api) {
    api.on("tool_call", |event, ctx| {
        if event["toolName"] != "write" && event["toolName"] != "edit" {
            return Ok(None);
        }
        let path = event["input"]["path"].as_str().unwrap_or_default();
        if !PROTECTED.iter().any(|protected| path.contains(protected)) {
            return Ok(None);
        }
        if ctx.has_ui() {
            notify(
                &format!("Blocked write to protected path: {path}"),
                "warning",
            );
        }
        Ok(Some(json!({
            "block": true,
            "reason": format!("Path \"{path}\" is protected"),
        })))
    });
}

ri_extension_api::extension!(init);
