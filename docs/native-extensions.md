# Native extensions in Rust

Native extensions are WebAssembly components written in Rust with the `yapi-extension-api` crate. They register tools, commands, flags and event handlers like Pi extensions, and use Pi's JSON shapes for events and results.

A native extension needs no JavaScript runtime and runs in a WebAssembly instance of its own. A built extension is one `.wasm` file of a few hundred kilobytes.

Native WebAssembly extensions are unstable until yapi 1.0. The WIT world, the Rust SDK and the host requests they use may change in any release before then, and extensions may need to be rebuilt. Pi extensions from npm use Pi's extension API and are not affected.

## Create an extension

Add the `wasm32-wasip2` target to your toolchain once:

```sh
rustup target add wasm32-wasip2
```

Create a project:

```sh
yapi new shout
```

`yapi new` writes a Cargo project that is also a yapi package:

```text
shout/
├── .cargo/config.toml   makes wasm32-wasip2 the default target
├── Cargo.toml           a cdylib crate that depends on yapi-extension-api at tag v0.1.0
├── README.md
├── extensions/          where the built extension goes
├── package.json         names extensions/shout.wasm for yapi
└── src/lib.rs           the hello example, to replace with your own
```

The package takes the directory's name, and `--name` sets another. The name has lowercase ASCII letters, digits, `-` and `_` and starts with a letter, so both Cargo and npm accept it. The same template works with [cargo-generate](https://github.com/cargo-generate/cargo-generate), which also asks for the name:

```sh
cargo generate --git https://github.com/SkymanOne/yapi --tag v0.1.0 crates/yapi/templates/extension
```

To start without the template, create a library crate with `crate-type = ["cdylib"]` and depend on the SDK at the yapi release you target:

```toml
[dependencies]
yapi-extension-api = { git = "https://github.com/SkymanOne/yapi", tag = "v0.1.0" }
```

An extension registers what it offers in an init function and exports it with `extension!`. Tools, commands and event handlers are `async` closures:

```rust
// src/lib.rs
use yapi_extension_api::{Api, Tool, ToolResult, json, notify};

fn init(api: &mut Api) {
    api.register_tool(Tool::new(
        "shout",
        "Repeats the text in capitals",
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        }),
        |params, _ctx| async move {
            let text = params["text"].as_str().unwrap_or_default();
            Ok(ToolResult::text(text.to_uppercase()))
        },
    ));
    api.register_command("hello", "Says hello", |args, _ctx| async move {
        let name = if args.is_empty() { "world" } else { &args };
        notify(&format!("Hello, {name}!"), "info");
        Ok(())
    });
}

yapi_extension_api::extension!(init);
```

Build it, and copy the build into the package:

```sh
cargo build --release
cp target/wasm32-wasip2/release/shout.wasm extensions/
```

A crate without the template's `.cargo/config.toml` builds with `cargo build --release --target wasm32-wasip2`.

## Try it

Load the package for a single run without installing it:

```sh
yapi -e .
```

`-e` also takes the `.wasm` file itself.

To check that the package loads without a model or the network, run [`check-extension.py`](https://github.com/SkymanOne/yapi/blob/main/skills/yapi-extension/scripts/check-extension.py) from the [agent skills](agent-skills.md). It prints the commands the extensions registered and every load error, and exits with 1 on an error:

```sh
python3 check-extension.py .
```

## Install it

`yapi install` takes the package folder or the `.wasm` file:

```sh
yapi install .       # for every project
yapi install . -l    # for this project only
```

yapi records the path in `settings.json` and loads the file from where it is, so a rebuild takes effect the next time yapi starts. `yapi list` shows the installed extension and `yapi remove <path>` uninstalls it. `yapi config` turns it off without removing it.

## Share it

A native extension is shared as a package that contains the built `.wasm` file. A project from `yapi new` already is one: commit `extensions/shout.wasm` with the sources, push the repository, and others install it by its URL:

```sh
yapi install git:github.com/you/shout
```

To publish on npm, run `npm publish`, then install with `yapi install npm:shout`. The template's `package.json` publishes only the `extensions` folder.

A package needs no manifest, because yapi loads every `.wasm` file in a package's `extensions` folder. A manifest names the files, and yapi reads its `yapi` key before the `pi` key, so one package can ship a native build for yapi and a JavaScript build for Pi:

```json
{
  "name": "shout",
  "version": "0.1.0",
  "pi": { "extensions": ["./dist/shout.js"] },
  "yapi": { "extensions": ["./dist/shout.wasm"] }
}
```

Commit or publish the built `.wasm` file, not only the Rust sources. yapi installs prebuilt files and never compiles code during installation, because a build runs `build.rs` scripts and procedural macros outside the sandbox. For the same reason yapi skips npm lifecycle scripts.

## API

| Item | Purpose |
|---|---|
| `Api::register_tool` | Offer a tool to the model. `Tool` sets a label, prompt snippet and prompt guidelines. |
| `Api::register_command` | Handle `/name args` |
| `Api::register_flag` | Accept `--name` on the command line, read later with `get_flag` |
| `Api::on` | Handle a Pi event such as `session_start` or `tool_call`. The return value is the handler's result in Pi, for example `{"block": true, "reason": "..."}`. A handler gets a copy of the event, so a `tool_call` handler changes the call's arguments by returning them as `input` instead of editing them. |
| `notify` | Show a notification |
| `send_message`, `append_entry` | Add a custom message or entry to the session |
| `exec` | Run a process and wait for it |
| `Process` | Run a process in the background, reading its output as it arrives |
| `sleep` | Wait a number of milliseconds |
| `spawn` | Run a future in the background, after the handler that started it has returned |
| `request` | Call any host action by name with a JSON payload and get its answer at once |
| `op` | Start a host operation by name, such as a dialog or an HTTP request, and await its answer |
| `Context` | The mode, the working folder and the rest of Pi's `ctx`. `Context::update` shows a running tool's progress. |

[Native extension examples](native-examples.md) walks through six complete extensions: a guard for dangerous commands, protected paths, a todo list kept per session branch, a git status reporter, a subagent that runs in the foreground or the background, and a minimal starting point.

### Host requests

`request(kind, payload)` sends a request to yapi and returns its JSON answer. yapi supports these kinds for native extensions:

| Kind | Payload | Answer |
|---|---|---|
| `log` | `{"level", "message"}`, with `level` one of `debug`, `info`, `warn` and `error` | `null`. yapi treats the message as console output from a Pi extension. |
| `cwd` | `{}` | The working folder extensions see, as a string |
| `exec.sync` | `{"command", "args", "cwd", "env", "input", "timeout"}`, all optional except `command` | `{"stdout", "stderr", "code", "signal", "killed"}` once the process exits |
| `execPath` | `{}` | The path of the running yapi binary, to start another yapi |
| `ui.notify` | `{"message", "type"}`, with `type` one of `info`, `warning` and `error`, or left out | `null`, as Pi's `ctx.ui.notify` |
| `session.sendMessage` | `{"message": {"customType", "content", "display", "details"}, "options": {"triggerTurn", "deliverAs"}}` | `null`, as Pi's `pi.sendMessage` |
| `session.appendEntry` | `{"customType", "data"}` | `null`, as Pi's `pi.appendEntry` |
| `session.read` | `{"method", "args"}` | The result of Pi's `ctx.sessionManager.<method>(...args)` |

`exec.sync` runs `command` with the strings in `args` and waits for it. Without `cwd` the process starts in yapi's working folder. `env` replaces the whole environment, `input` is written to standard input, and `timeout` kills the process after that many milliseconds. `code` is `null` when a signal ended the process, and `killed` is `true` when the timeout did. A process that cannot start answers `{"stdout": "", "stderr": "", "code": null, "error"}`, where `error` is Node's spawn error, such as `spawn git ENOENT`. The request fails when the extension may not run processes.

`session.read` takes the method's arguments as the array `args`, such as `["<entry id>"]` for `getEntry`. The methods are `getCwd`, `getSessionDir`, `getSessionId`, `getSessionFile`, `getSessionName`, `isPersisted`, `getHeader`, `getEntries`, `getEntry`, `getChildren`, `getLabel`, `getLeafId`, `getLeafEntry`, `getBranch` and `buildSessionContext`. Entries have the shapes of Pi's session files.

`ui.notify` and the `session` requests fail while the init function runs, before yapi binds the extension to a session. Every other kind is internal to yapi and may change or disappear in any release.

### Host operations

`op(kind, payload)` starts an operation and returns a future of its JSON answer. Other handlers and background tasks run while it waits. yapi supports these kinds for native extensions:

| Kind | Payload | Answer |
|---|---|---|
| `timer` | `{"ms"}` | `null` after `ms` milliseconds. `sleep` wraps it. |
| `fetch` | `{"url", "method", "headers", "body"}`, with `bodyBase64` in place of `body` for bytes | `{"status", "statusText", "headers", "bodyBase64"}` once the whole response has arrived |
| `ui.select` | `{"title", "options"}` | The option picked, or `null` when cancelled, as Pi's `ctx.ui.select` |
| `ui.confirm` | `{"title", "message"}` | `true` or `false`, as Pi's `ctx.ui.confirm` |
| `ui.input` | `{"title", "placeholder"}` | The text entered, or `null` when cancelled, as Pi's `ctx.ui.input` |

`fetch` fails when the extension may not use the network. The dialogs answer `null` or `false` at once in print and JSON modes, which have no interface, so check `Context::has_ui` first. Every other kind is internal to yapi and may change or disappear in any release.

### Processes

`Process::spawn` starts a process that runs while the extension does other work. It takes `{"command", "args", "cwd", "env", "stdin"}`, all optional except `command`. Without `cwd` the process starts in yapi's working folder, `env` replaces the whole environment, and `"stdin": "ignore"` gives it empty standard input instead of a pipe. `Process::next` waits for the next `ProcessEvent`: output on standard output or standard error, then the exit. `write`, `close_stdin` and `kill` act on the running process.

Dropping a `Process` kills it, and so does stopping the extension. Starting one needs the process grant, which every extension has by default.

## Background work and cancellation

Handlers are `async`. While one waits for a host operation, other handlers, tool calls and background tasks run. `spawn` runs a future in the background, after the handler that started it has returned, as a Pi extension does with a promise it does not await. A background task can report its result with the `session.sendMessage` request, with `"triggerTurn": true` to start a turn when the agent is idle.

When the user aborts a run, yapi drops the futures of the extension tools still running in it, which kills the processes they hold. Background tasks keep running until they finish or the extension stops.

## Limits

Extension UI components are available to Pi extensions only. Each call into an extension may compute for 60 seconds without waiting on yapi, and each extension may use 1 GiB of memory.
