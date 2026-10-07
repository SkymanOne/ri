//! An example native extension: a tool, a command, a flag and two event
//! handlers.

use yapi_extension_api::{Api, FlagType, Tool, ToolResult, append_entry, get_flag, json, notify};

fn init(api: &mut Api) {
    api.register_flag(
        "shout-suffix",
        FlagType::String,
        json!("!"),
        "Text after shouted words",
    );
    api.register_tool(
        Tool::new(
            "shout",
            "Repeats the text in capitals",
            json!({
                "type": "object",
                "properties": {"text": {"type": "string", "description": "What to shout"}},
                "required": ["text"],
            }),
            |params, _ctx| async move {
                let text = params["text"].as_str().unwrap_or_default().to_uppercase();
                let suffix = get_flag("shout-suffix")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                Ok(ToolResult::text(format!("{text}{suffix}"))
                    .with_details(json!({"length": text.len()})))
            },
        )
        .label("Shout")
        .prompt_snippet("Shout text back in capitals"),
    );
    api.register_command("hello", "Says hello", |args, _ctx| async move {
        let name = if args.is_empty() { "world" } else { &args };
        notify(&format!("Hello, {name}!"), "info");
        Ok(())
    });
    api.on("session_start", |_event, ctx| async move {
        append_entry("hello-started", json!({"mode": ctx.mode()}))?;
        Ok(None)
    });
    api.on("tool_call", |event, _ctx| async move {
        if event["toolName"] == "shout" && event["input"]["text"] == "" {
            return Ok(Some(json!({"block": true, "reason": "Nothing to shout"})));
        }
        Ok(None)
    });
}

yapi_extension_api::extension!(init);
