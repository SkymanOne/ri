//! Asks before dangerous bash commands run: recursive deletes, `sudo`, and
//! `chmod` or `chown` with `777`. Without a person to ask, it blocks them.
//!
//! A port of pi's `permission-gate.ts` example, with the same dialog and
//! messages.

use yapi_extension_api::{Api, json, op};

/// Whether `command` matches pi's patterns: `rm -r…` or `rm --recursive`,
/// `sudo`, and `chmod` or `chown` followed later by `777`.
fn is_dangerous(command: &str) -> bool {
    let words: Vec<String> = command.split_whitespace().map(str::to_lowercase).collect();
    let recursive_delete = words
        .windows(2)
        .any(|pair| pair[0] == "rm" && (pair[1].starts_with("-r") || pair[1] == "--recursive"));
    let sudo = words.iter().any(|word| word == "sudo");
    let open_permissions = words
        .iter()
        .position(|word| word == "chmod" || word == "chown")
        .is_some_and(|start| words[start..].iter().any(|word| word.contains("777")));
    recursive_delete || sudo || open_permissions
}

fn init(api: &mut Api) {
    api.on("tool_call", |event, ctx| async move {
        let command = event["input"]["command"].as_str().unwrap_or_default();
        if event["toolName"] != "bash" || !is_dangerous(command) {
            return Ok(None);
        }
        if !ctx.has_ui() {
            return Ok(Some(json!({
                "block": true,
                "reason": "Dangerous command blocked (no UI for confirmation)",
            })));
        }
        let title = format!("⚠️ Dangerous command:\n\n  {command}\n\nAllow?");
        let choice = op(
            "ui.select",
            &json!({"title": title, "options": ["Yes", "No"]}),
        )
        .await?;
        if choice != "Yes" {
            return Ok(Some(json!({"block": true, "reason": "Blocked by user"})));
        }
        Ok(None)
    });
}

yapi_extension_api::extension!(init);
