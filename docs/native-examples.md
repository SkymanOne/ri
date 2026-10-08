# Native extension examples

The repository has seven native extensions in [`guest/examples`](https://github.com/SkymanOne/yapi/tree/main/guest/examples). Six are ports of Pi's own examples, so the Rust and TypeScript versions can be read side by side. `subagent` is tested against a real yapi in `crates/yapi/tests/subagent.rs`, and the others in `crates/yapi-ext/tests/native.rs`. Terminal scenarios compare the screens of `todo` and `custom-footer` with Pi running the TypeScript versions.

| Example | Shows | Pi counterpart |
|---|---|---|
| [`hello`](#hello) | A tool, a command, a flag and two event handlers | `hello.ts` |
| [`permission-gate`](#permission-gate) | Blocking tool calls from a `tool_call` handler, asking in a dialog | `permission-gate.ts` |
| [`protected-paths`](#protected-paths) | Inspecting tool input, notifications | `protected-paths.ts` |
| [`todo`](#todo) | State kept in tool results and rebuilt from the session branch, a component that lists it | `todo.ts` |
| [`custom-footer`](#custom-footer) | A component in place of the footer that reads the session as it renders | `custom-footer.ts` |
| [`repo-status`](#repo-status) | Running processes with `exec`, a startup warning | None |
| [`subagent`](#subagent) | A process read as it runs, tool progress, background work that reports in a new turn | `subagent/` |

## Build and try them

Build any example from the `guest` folder of a clone, then load it for one run:

```sh
cd guest
cargo build --release --target wasm32-wasip2 -p permission-gate
yapi -e target/wasm32-wasip2/release/permission_gate.wasm
```

Cargo names the file after the crate with underscores in place of dashes. `yapi install` takes the same path to keep the extension.

## hello

The smallest complete extension. It registers a `shout` tool that repeats text in capitals, a `/hello` command, a `--shout-suffix` flag that the tool reads, a `session_start` handler that stores an entry in the session, and a `tool_call` handler that blocks empty input.

```rust
api.on("tool_call", |event, _ctx| async move {
    if event["toolName"] == "shout" && event["input"]["text"] == "" {
        return Ok(Some(json!({"block": true, "reason": "Nothing to shout"})));
    }
    Ok(None)
});
```

## permission-gate

Asks before dangerous `bash` commands run: recursive deletes, `sudo`, and `chmod` or `chown` with `777`. Returning `{"block": true, "reason": ...}` from a `tool_call` handler stops the call, and the model sees the reason as the tool's result. The handler awaits a dialog with `op`, and blocks the command when no one can answer, in print and JSON modes.

```rust
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
    let choice = op("ui.select", &json!({"title": title, "options": ["Yes", "No"]})).await?;
    if choice != "Yes" {
        return Ok(Some(json!({"block": true, "reason": "Blocked by user"})));
    }
    Ok(None)
});
```

## protected-paths

Blocks `write` and `edit` calls on `.env`, `.git/` and `node_modules/`, and tells the user when a person is watching.

```rust
const PROTECTED: [&str; 3] = [".env", ".git/", "node_modules/"];

api.on("tool_call", |event, ctx| async move {
    if event["toolName"] != "write" && event["toolName"] != "edit" {
        return Ok(None);
    }
    let path = event["input"]["path"].as_str().unwrap_or_default();
    if !PROTECTED.iter().any(|protected| path.contains(protected)) {
        return Ok(None);
    }
    if ctx.has_ui() {
        notify(&format!("Blocked write to protected path: {path}"), "warning");
    }
    Ok(Some(json!({"block": true, "reason": format!("Path \"{path}\" is protected")})))
});
```

## todo

A `todo` tool with `list`, `add`, `toggle` and `clear` actions, and a `/todos` command that shows the list in a component until Escape closes it. Each result carries the whole list in its details. When a session starts, or the user moves through the session tree with `/tree`, the extension reads the current branch and takes the list from the last `todo` result on it. Every branch therefore keeps its own list.

```rust
fn rebuild() -> Result<(), String> {
    let branch = request("session.read", &json!({"method": "getBranch", "args": []}))?;
    let mut state = State { todos: Vec::new(), next_id: 1 };
    for entry in branch.as_array().into_iter().flatten() {
        let message = &entry["message"];
        if entry["type"] == "message" && message["role"] == "toolResult" && message["toolName"] == "todo" {
            let details = &message["details"];
            state.todos = details["todos"].as_array().cloned().unwrap_or_default();
            state.next_id = details["nextId"].as_u64().unwrap_or(1);
        }
    }
    STATE.with(|cell| *cell.borrow_mut() = state);
    Ok(())
}

api.on("session_start", |_event, _ctx| async { rebuild().map(|()| None) });
api.on("session_tree", |_event, _ctx| async { rebuild().map(|()| None) });
```

The extension keeps its state in a `thread_local`, since each native extension runs in an instance of its own and calls into it one at a time.

`/todos` shows the list with `custom`, which resolves once the component calls `finish` on its `Done`. Outside the interactive mode the command reports an error instead, as Pi's does.

```rust
impl Component for TodoList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let th = theme();
        // ...
    }

    fn handle_input(&mut self, data: &str) {
        if matches!(parse_key(data).as_deref(), Some("escape" | "ctrl+c")) {
            self.done.finish(());
        }
    }
}

let todos = STATE.with(|cell| cell.borrow().todos.clone());
ctx.custom(|done| TodoList { todos, done }, CustomOptions::default()).await;
```

## custom-footer

`/footer` replaces the footer with the session's token counts and cost on the left, and the model and the git branch on the right. Running it again restores the built-in footer. The footer is a component that reads the session's branch with `session.read` and the git branch with `ui.footerData` each time yapi renders it.

```rust
api.register_command("footer", "Toggle custom footer", |_args, ctx| async move {
    let enabled = !ENABLED.get();
    ENABLED.set(enabled);
    if enabled {
        let model = ctx.data()["model"]["id"].as_str().filter(|id| !id.is_empty());
        let model = model.unwrap_or("no-model").to_owned();
        ctx.set_footer(Some(Box::new(Footer { model })));
        notify("Custom footer enabled", "info");
    } else {
        ctx.set_footer(None);
        notify("Default footer restored", "info");
    }
    Ok(())
});
```

## repo-status

Runs `git` with `exec` to report the current branch and the files with uncommitted changes. It warns when a session starts in a repository with changes, answers `/repo-status`, and offers a `repo_status` tool to the model.

```rust
fn status(ctx: &Context) -> Option<(String, Vec<String>)> {
    let cwd = ctx.cwd();
    let branch = exec("git", &["branch", "--show-current"], cwd).ok()?;
    if branch.code != Some(0) {
        return None;
    }
    let changes = exec("git", &["status", "--porcelain"], cwd).ok()?;
    // ...
}
```

`exec` needs the process grant, which every extension has by default.

## subagent

A `subagent` tool that hands a task to another yapi run, with a context window of its own. The tool starts yapi in JSON mode with `Process` and reads its events as they arrive, as Pi's `subagent` example does with Pi. In the foreground the tool shows each answer of the subagent as progress and returns the last one. With `background`, the tool returns at once and `spawn` keeps the subagent running. Its answer arrives later as a message that starts a turn.

```rust
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
```

`request("execPath", ...)` names the running yapi binary, so the subagent runs the same version. When the user aborts the run, yapi drops the foreground tool's future, and dropping the `Process` kills the subagent. Pi's own TypeScript example also runs unchanged in yapi, with each agent defined in `~/.yapi/agent/agents`.
