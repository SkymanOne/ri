# Native extension examples

The repository has sixteen native extensions in [`guest/examples`](https://github.com/SkymanOne/yapi/tree/main/guest/examples). Eleven are ports of Pi's own examples, so the Rust and TypeScript versions can be read side by side. `subagent` is tested against a real yapi in `crates/yapi/tests/subagent.rs`, `event-bus` with Pi extensions in `crates/yapi-ext/tests/bus.rs`, and the others in `crates/yapi-ext/tests/native.rs`. Terminal scenarios also compare the screens of nine examples against Pi running the TypeScript versions.

| Example | Shows | Pi counterpart |
|---|---|---|
| [`hello`](#hello) | A tool, a command, a flag and two event handlers | `hello.ts` |
| [`permission-gate`](#permission-gate) | Blocking tool calls from a `tool_call` handler, asking in a dialog | `permission-gate.ts` |
| [`protected-paths`](#protected-paths) | Inspecting tool input, notifications | `protected-paths.ts` |
| [`todo`](#todo) | State kept in tool results and rebuilt from the session branch, a component that lists it | `todo.ts` |
| [`custom-footer`](#custom-footer) | A component in place of the footer that reads the session as it renders | `custom-footer.ts` |
| [`repo-status`](#repo-status) | Running processes with `exec`, a startup warning | None |
| [`event-bus`](#event-bus) | Events between extensions with `events`, native or Pi | `event-bus.ts` |
| [`package-scripts`](#package-scripts) | The extension's own location, variables added to a process's inherited environment | None |
| [`subagent`](#subagent) | A process read as it runs, tool progress, background work that reports in a new turn | `subagent/` |
| [`modal-editor`](#modal-editor) | An editor component in place of the built-in editor, wrapping `CustomEditor` | `modal-editor.ts` |
| [`question`](#question) | A tool that asks in a component with an `Editor` widget, and draws its own call and result | `question.ts` |
| [`overlay-test`](#overlay-test) | An overlay with inline inputs, the cursor marker and wide characters | `overlay-test.ts` |
| [`message-renderer`](#message-renderer) | Custom messages drawn in a box by a message renderer | `message-renderer.ts` |
| [`select-menu`](#select-menu) | A `SelectList` widget in a custom component | None |
| [`input-hooks`](#input-hooks) | A terminal input listener that reads the editor, an autocomplete provider and a shortcut | None |
| [`crash-recovery`](#crash-recovery) | What yapi does when an extension panics | None |

Five of them, from `modal-editor` to `select-menu`, use the SDK's `widgets` feature.

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

## event-bus

Extensions talking over Pi's `pi.events`. A listener shows every `my:notification` event, from this extension or any other, native or Pi. `/emit [message]` emits one, and so does the start of a session. `/mute` stops the listener with its `Subscription`, and starts it again. Pi's example has no `/mute`.

```rust
fn listen() -> Subscription {
    events::on("my:notification", |data| {
        let message = data["message"].as_str().unwrap_or_default();
        let from = data["from"].as_str().unwrap_or_default();
        notify(&format!("Event from {from}: {message}"), "info");
    })
}
```

The extension's own listener hears its event while `events::emit` runs. Pi extensions and other native extensions hear it right after.

## package-scripts

`/script <name> [args]` runs `scripts/<name>` with `sh` and shows what it printed. The script comes from the root of the package the extension was installed from, or from the folder of its `.wasm` file when it was given with `-e`. `extension_path` says where both are. The script gets `EXTENSION_DIR`, the folder it came from, on top of the environment it inherits, so it still finds programs on `PATH`:

```rust
let output = request(
    "exec.sync",
    &json!({
        "command": "sh",
        "args": command,
        "cwd": ctx.cwd(),
        "envAdd": {"EXTENSION_DIR": dir},
    }),
)?;
```

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

## modal-editor

Vim-like modes for the prompt editor. Escape switches from insert to normal mode, where `hjkl` move the cursor, `0` and `$` go to the line's start and end, `x` deletes, and `i` and `a` switch back. The extension puts an `EditorComponent` in place of the built-in editor when the session starts. It wraps `widgets::CustomEditor`, which keeps the built-in editor's keys, history and completions, and turns normal mode's keys into the keys the editor knows.

```rust
fn handle_input(&mut self, data: &str) {
    if parse_key(data).as_deref() == Some("escape") {
        if self.normal {
            self.editor.handle_input(data);
        } else {
            self.normal = true;
        }
        return;
    }
    // ...
}

api.on("session_start", |_event, ctx| async move {
    ctx.set_editor_component(Some(Box::new(ModalEditor::default())));
    Ok(None)
});
```

## question

A `question` tool for the model: the user picks one of its options in a component, or chooses "Type something." and writes an answer in an `Editor` widget. `render_call` and `render_result` draw the question and the answer in the transcript with `widgets::Text`.

```rust
.render_result(|result, _options, _ctx| {
    let th = theme();
    let details = &result["details"];
    // ...
    Some(Box::new(Text::new(text, 0, 0)))
})
```

## overlay-test

`/overlay-test` opens an overlay with inline text inputs and lines of wide characters, styled text and emoji. The component's `width` sets the overlay's, and the selected input puts pi-tui's cursor marker where the terminal's cursor belongs. `widgets::visible_width` pads styled text to the box's width.

## message-renderer

`/status [warn|error] message` adds a custom message that a message renderer draws in a box, colored by level. The box uses the `boxed` and `text` layouts of `widgets::tui::lines` over the theme's custom message background.

## select-menu

`/menu` opens a drinks menu: a `SelectList` between two borders, which reports the pick in a notification. It ports the TypeScript menu that yapi's own terminal scenarios use.

```rust
fn handle_input(&mut self, data: &str) {
    match self.list.handle_input(data, &keybindings()) {
        SelectEvent::Selected(item) => self.done.finish(Some(item.value)),
        SelectEvent::Cancelled => self.done.finish(None),
        SelectEvent::Moved | SelectEvent::Ignored => {}
    }
}
```

## input-hooks

A terminal input listener sees every key before the editor does. In an empty prompt editor `?` shows help instead of being typed, and `editor_focused()` leaves it to dialogs and selectors. `a` becomes `A`, and Ctrl+G reports how many keys the listener saw and what the editor holds. `/quiet` drops the listener's `Subscription`, which stops it. An autocomplete provider completes environment variable names after `$` and leaves other text to the providers it wraps, and Alt+K is a shortcut that reports the count too. Its terminal scenarios run the same extension in TypeScript in Pi.

```rust
fn listen(data: &str) -> TerminalInput {
    SEEN.set(SEEN.get() + 1);
    if data == "?" && editor_focused() && editor_text().is_empty() {
        notify("Type $ for variables, Alt+K to count keys", "info");
        return TerminalInput::Consume;
    }
    if data == "a" {
        return TerminalInput::Replace("A".into());
    }
    // ...
    TerminalInput::Pass
}

api.on("session_start", |_event, ctx| async move {
    LISTENER.set(Some(ctx.on_terminal_input(listen)));
    ctx.add_autocomplete_provider(Variables);
    Ok(None)
});
```

## crash-recovery

An editor in place of the built-in one, and a `/fragile` dialog, that panic when their text is `panic`. A panic stops the extension's runtime. yapi reports it, restarts the runtime and loads the extension again, without what it showed. The dialog closes and the built-in editor takes the keys. The editor that `session_start` installed stays away until the next session.

```rust
fn render(&mut self, _width: usize) -> Vec<String> {
    assert!(self.text != "panic", "asked to panic");
    vec![format!("> {}", self.text)]
}
```
