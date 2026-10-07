//! A `subagent` tool that delegates a task to another yapi run with a
//! context window of its own. In the foreground the tool shows the
//! subagent's progress and returns its answer. In the background it returns
//! at once, and the answer arrives later as a message that starts a turn.
//!
//! A minimal take on pi's `subagent` example, which runs pi the same way:
//! in JSON mode, reading its events as they arrive.

use yapi_extension_api::{
    Api, Process, ProcessEvent, Tool, ToolResult, Value, json, request, spawn,
};

/// The text of an assistant message.
fn text(message: &Value) -> String {
    let blocks = message["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let texts: Vec<&str> = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    texts.join("\n")
}

/// Runs `task` in a new yapi process in `cwd`, with `model` when given, and
/// returns its last answer. `progress` sees each answer as it arrives.
/// Dropping the future kills the process.
async fn run(
    task: &str,
    model: Option<&str>,
    cwd: Option<&str>,
    progress: impl Fn(&str),
) -> Result<String, String> {
    let yapi = request("execPath", &json!({}))?;
    let mut args = vec!["--mode", "json", "-p", "--no-session"];
    if let Some(model) = model {
        args.extend(["--model", model]);
    }
    args.push(task);
    let mut process = Process::spawn(&json!({
        "command": yapi,
        "args": args,
        "cwd": cwd,
        "stdin": "ignore",
    }))?;
    let (mut output, mut errors, mut answer, mut code) =
        (Vec::new(), Vec::new(), String::new(), None);
    while let Some(event) = process.next().await {
        match event {
            ProcessEvent::Stdout(bytes) => output.extend(bytes),
            ProcessEvent::Stderr(bytes) => errors.extend(bytes),
            ProcessEvent::Exit { code: exit, .. } => code = exit,
        }
        // Each line is one event.
        while let Some(end) = output.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = output.drain(..=end).collect();
            let Ok(event) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if event["type"] == "message_end" && event["message"]["role"] == "assistant" {
                answer = text(&event["message"]);
                progress(&answer);
            }
        }
    }
    if code != Some(0) {
        return Err(format!(
            "The subagent failed: {}",
            String::from_utf8_lossy(&errors).trim()
        ));
    }
    Ok(answer)
}

fn init(api: &mut Api) {
    api.register_tool(
        Tool::new(
            "subagent",
            "Delegate a task to a subagent with its own context window. With background, return at once; the answer arrives later as a message.",
            json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The task, with everything the subagent needs to know"},
                    "background": {"type": "boolean", "description": "Return at once and deliver the answer as a message"},
                    "model": {"type": "string", "description": "The subagent's model, such as anthropic/claude-sonnet-4-5"},
                },
                "required": ["task"],
            }),
            |params, ctx| async move {
                let task = params["task"].as_str().unwrap_or_default().to_owned();
                let model = params["model"].as_str().map(str::to_owned);
                let cwd = ctx.cwd().map(str::to_owned);
                if params["background"] != true {
                    let progress = |answer: &str| ctx.update(&ToolResult::text(answer));
                    let answer = run(&task, model.as_deref(), cwd.as_deref(), progress).await?;
                    return Ok(ToolResult::text(answer));
                }
                spawn(async move {
                    let content = match run(&task, model.as_deref(), cwd.as_deref(), |_| {}).await {
                        Ok(answer) => format!("Subagent finished:\n\n{answer}"),
                        Err(error) => error,
                    };
                    let _ = request(
                        "session.sendMessage",
                        &json!({
                            "message": {"customType": "subagent", "content": content, "display": true},
                            "options": {"triggerTurn": true, "deliverAs": "followUp"},
                        }),
                    );
                });
                Ok(ToolResult::text(
                    "The subagent is running in the background. Its answer will arrive as a message.",
                ))
            },
        )
        .label("Subagent"),
    );
}

yapi_extension_api::extension!(init);
