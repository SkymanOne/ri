# Native extension examples

The repository has five native extensions in [`guest/examples`](https://github.com/SkymanOne/yapi/tree/main/guest/examples). Four are ports of Pi's own examples, so the Rust and TypeScript versions can be read side by side. Each one is tested in `crates/yapi-ext/tests/native.rs`.

| Example | Shows | Pi counterpart |
|---|---|---|
| [`hello`](#hello) | A tool, a command, a flag and two event handlers | `hello.ts` |
| [`permission-gate`](#permission-gate) | Blocking tool calls from a `tool_call` handler, a boolean flag | `permission-gate.ts` |
| [`protected-paths`](#protected-paths) | Inspecting tool input, notifications | `protected-paths.ts` |
| [`todo`](#todo) | State kept in tool results and rebuilt from the session branch | `todo.ts` |
| [`repo-status`](#repo-status) | Running processes with `exec`, a startup warning | None |

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
api.on("tool_call", |event, _ctx| {
    if event["toolName"] == "shout" && event["input"]["text"] == "" {
        return Ok(Some(json!({"block": true, "reason": "Nothing to shout"})));
    }
    Ok(None)
});
```

## permission-gate

Blocks dangerous `bash` commands: recursive deletes, `sudo`, and `chmod` or `chown` with `777`. Returning `{"block": true, "reason": ...}` from a `tool_call` handler stops the call, and the model sees the reason as the tool's result. Starting yapi with `--allow-dangerous` turns the check off.

```rust
api.register_flag("allow-dangerous", FlagType::Boolean, json!(false), "Let dangerous bash commands run");
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
        "reason": "Dangerous command blocked. Start yapi with --allow-dangerous to allow it.",
    })))
});
```

Pi's version asks for confirmation in a dialog. Native handlers run synchronously, so this one blocks instead.

## protected-paths

Blocks `write` and `edit` calls on `.env`, `.git/` and `node_modules/`, and tells the user when a person is watching.

```rust
const PROTECTED: [&str; 3] = [".env", ".git/", "node_modules/"];

api.on("tool_call", |event, ctx| {
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

A `todo` tool with `list`, `add`, `toggle` and `clear` actions, and a `/todos` command that shows the list. Each result carries the whole list in its details. When a session starts, or the user moves through the session tree with `/tree`, the extension reads the current branch and takes the list from the last `todo` result on it. Every branch therefore keeps its own list.

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

api.on("session_start", |_event, _ctx| rebuild().map(|()| None));
api.on("session_tree", |_event, _ctx| rebuild().map(|()| None));
```

The extension keeps its state in a `thread_local`, since each native extension runs in an instance of its own and calls into it one at a time.

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
