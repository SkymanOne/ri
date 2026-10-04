//! Blocks dangerous bash commands: recursive deletes, `sudo`, and `chmod` or
//! `chown` with `777`. `--allow-dangerous` lets them run.
//!
//! A port of pi's `permission-gate.ts` example. pi asks for confirmation in
//! the terminal; native handlers answer at once, so this one blocks.

use ri_extension_api::{Api, FlagType, Value, get_flag, json};

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
    api.register_flag(
        "allow-dangerous",
        FlagType::Boolean,
        json!(false),
        "Let dangerous bash commands run",
    );
    api.on("tool_call", |event, _ctx| {
        if event["toolName"] != "bash" || get_flag("allow-dangerous") == Some(Value::Bool(true)) {
            return Ok(None);
        }
        let command = event["input"]["command"].as_str().unwrap_or_default();
        if !is_dangerous(command) {
            return Ok(None);
        }
        Ok(Some(json!({
            "block": true,
            "reason": "Dangerous command blocked. Start ri with --allow-dangerous to allow it.",
        })))
    });
}

ri_extension_api::extension!(init);
